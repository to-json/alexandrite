//! Probe 06, hand-emitted. `#[pure]` as destination-passing style.
//!
//! Every pure fn takes its args plus a caller-allocated destination sized by
//! the header's bound. Each proof obligation is tagged with how it was
//! discharged:
//!   [interval]    simple range arithmetic on types/constants
//!   [structural]  follows from the shape of the code (map keeps length, ...)
//!   [relational]  needs a fact relating two values (sum vs count)
//!   [domain]      genuinely can fail on some input -> fallible
//!   [prover]      true or false depending on data; a better prover can't help
//!
//! Two emissions: `dps` (ordinary fns) and `konst` (const fn, so rustc
//! enforces part of purity). `--features violations` adds const fns that
//! break purity and should be rejected.
#![feature(const_trait_impl, const_ops)]

use alx_rt::{Counting, Id, Pool};

#[global_allocator]
static A: Counting = Counting;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PureError {
    Overflow,
    BadDigit(u8),
}

/// `NonEmpty[I32]`: parse, don't validate. The only way in is `new`, which
/// is where the caller (impure) handles emptiness.
#[derive(Clone, Copy)]
pub struct NonEmpty<'a>(&'a [i32]);

impl<'a> NonEmpty<'a> {
    pub const fn new(xs: &'a [i32]) -> Option<Self> {
        if xs.is_empty() { None } else { Some(NonEmpty(xs)) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tree {
    Leaf(i64),
    Node(Id<Tree>, Id<Tree>),
}

mod dps {
    use super::*;

    /// sum(xs: [I64]) -> I64!
    /// - overflow of the accumulator: [prover] depends on data -> fallible.
    pub fn sum(xs: &[i64], out: &mut i64) -> Result<(), PureError> {
        let mut acc: i64 = 0;
        for &x in xs {
            acc = acc.checked_add(x).ok_or(PureError::Overflow)?;
        }
        *out = acc;
        Ok(())
    }

    /// doubled(xs: [I32]) -> [I64], size out == size xs
    /// - `x as i64 * 2` fits: [interval] |x| <= 2^31, 2^32 < 2^63.
    /// - write in range: [structural] map preserves length; out.len() == xs.len()
    ///   is the header's bound, which the caller established.
    pub fn doubled(xs: &[i32], out: &mut [i64]) {
        for (o, &x) in out.iter_mut().zip(xs) {
            *o = x as i64 * 2;
        }
    }

    /// parse_digits(s: Str) -> [U8]!, size out <= size s
    /// - non-digit byte: [domain] -> fallible.
    /// - write in range: [structural] one output per input byte.
    ///   Returns the filled length.
    pub fn parse_digits(s: &[u8], out: &mut [u8]) -> Result<usize, PureError> {
        for (o, &b) in out.iter_mut().zip(s) {
            if !b.is_ascii_digit() {
                return Err(PureError::BadDigit(b));
            }
            *o = b - b'0';
        }
        Ok(s.len())
    }

    /// mirror(t: Tree) -> Tree, size out == size t
    /// - dst never outgrows its capacity: [structural] one node out per node
    ///   in; caller sized dst with Pool::exact(size t).
    pub fn mirror(src: &Pool<Tree>, t: Id<Tree>, dst: &mut Pool<Tree>) -> Id<Tree> {
        match *src.get(t) {
            Tree::Leaf(n) => dst.put(Tree::Leaf(n)),
            Tree::Node(l, r) => {
                let r2 = mirror(src, r, dst);
                let l2 = mirror(src, l, dst);
                dst.put(Tree::Node(r2, l2))
            }
        }
    }

    /// average(xs: NonEmpty[I32]) -> I32
    /// - i128 accumulator can't overflow: [interval] n <= 2^61 (slice limit
    ///   for 4-byte elements), |x| <= 2^31, so |sum| <= 2^92.
    /// - division by zero: [interval] n >= 1 by NonEmpty.
    /// - result fits i32: [relational] min <= sum/n <= max. An interval-only
    ///   prover sees sum in [n*min, n*max] and n in [1, 2^61] separately and
    ///   can't conclude this; it needs the fact that ties sum to n.
    pub fn average(xs: NonEmpty, out: &mut i32) {
        let mut acc: i128 = 0;
        for &x in xs.0 {
            acc += x as i128;
        }
        *out = (acc / xs.0.len() as i128) as i32;
    }

    /// median(xs: NonEmpty[I32]) -> I32
    /// - index n/2 < n: [interval] n >= 1.
    /// - `sorted` is scratch: allocated here, dropped before return. The
    ///   caller sees no net allocation.
    pub fn median(xs: NonEmpty, out: &mut i32) {
        let mut scratch = xs.0.to_vec();
        scratch.sort_unstable();
        *out = scratch[scratch.len() / 2];
    }
}

/// The same functions as `const fn`. rustc then rejects I/O, heap allocation
/// and non-const calls inside them. `for` loops aren't allowed (iterator
/// traits aren't const), so the emitter writes index loops.
mod konst {
    use super::*;
    use core::ops::Add;

    pub const fn sum(xs: &[i64], out: &mut i64) -> Result<(), PureError> {
        let mut acc: i64 = 0;
        let mut i = 0;
        while i < xs.len() {
            acc = match acc.checked_add(xs[i]) {
                Some(v) => v,
                None => return Err(PureError::Overflow),
            };
            i += 1;
        }
        *out = acc;
        Ok(())
    }

    pub const fn doubled(xs: &[i32], out: &mut [i64]) {
        let mut i = 0;
        while i < xs.len() {
            out[i] = xs[i] as i64 * 2;
            i += 1;
        }
    }

    pub const fn parse_digits(s: &[u8], out: &mut [u8]) -> Result<usize, PureError> {
        let mut i = 0;
        while i < s.len() {
            let b = s[i];
            if !b.is_ascii_digit() {
                return Err(PureError::BadDigit(b));
            }
            out[i] = b - b'0';
            i += 1;
        }
        Ok(s.len())
    }

    pub const fn average(xs: NonEmpty, out: &mut i32) {
        let mut acc: i128 = 0;
        let mut i = 0;
        while i < xs.0.len() {
            acc += xs.0[i] as i128;
            i += 1;
        }
        *out = (acc / xs.0.len() as i128) as i32;
    }

    // mirror: impossible as const fn. Pool::put is Vec::push (not const).
    // median: impossible as const fn. Needs heap scratch and sort (not const).

    /// Generic pure code over const traits (nightly `[const]` bounds).
    pub const fn sum3<T: [const] Add<Output = T> + Copy>(a: T, b: T, c: T) -> T {
        a + b + c
    }

    /// Evaluated at compile time: proof that these really are const.
    pub const COMPILE_TIME: (i64, i32, u8) = {
        let mut s = 0;
        let _ = sum(&[1, 2, 3], &mut s);
        let mut a = 0;
        average(NonEmpty(&[10, 20, 31]), &mut a);
        (s, a, sum3(1u8, 2, 3))
    };

    /// What const fn does NOT catch: panics. `xs[9]` compiles fine here and
    /// panics at runtime (or fails const-eval if called at compile time).
    pub const fn panics(xs: &[i32]) -> i32 {
        xs[9]
    }
}

#[cfg(feature = "violations")]
mod violations {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static mut COUNTER: usize = 0;
    static ATOMIC: AtomicUsize = AtomicUsize::new(0);
    static PLAIN: usize = 7;

    pub const fn io() {
        println!("side effect");
    }
    pub const fn heap() -> usize {
        let mut v = Vec::new();
        v.push(1);
        v.len()
    }
    pub const fn boxed() -> Box<i32> {
        Box::new(1)
    }
    pub const fn static_mut() -> usize {
        unsafe { COUNTER }
    }
    pub const fn atomic() -> usize {
        ATOMIC.load(Ordering::Relaxed)
    }
    pub const fn plain_static() -> usize {
        PLAIN
    }
    pub const fn mutate_arg(xs: &mut Vec<i32>) {
        xs.clear();
    }
    // Accepted (see NOTES): `static_mut` reads mutable global state at
    // runtime and only fails if const-evaluated; `plain_static` is fine
    // (immutable); `&mut [T]` args can be written through.
}

/// Call `f` after the caller has allocated the destination. Report what the
/// call itself allocated and the net change in live bytes.
fn measure<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let (a0, l0) = (alx_rt::alloc_count(), alx_rt::live_bytes());
    let r = f();
    let (a1, l1) = (alx_rt::alloc_count(), alx_rt::live_bytes());
    println!("  {label:<14} allocs_during_call={:<2} net_live_bytes={}", a1 - a0, l1 as isize - l0 as isize);
    r
}

fn build(p: &mut Pool<Tree>, depth: u32, next: &mut i64) -> Id<Tree> {
    if depth == 0 {
        *next += 1;
        return p.put(Tree::Leaf(*next));
    }
    let l = build(p, depth - 1, next);
    let r = build(p, depth - 1, next);
    p.put(Tree::Node(l, r))
}

fn leaves(p: &Pool<Tree>, t: Id<Tree>, out: &mut Vec<i64>) {
    match *p.get(t) {
        Tree::Leaf(n) => out.push(n),
        Tree::Node(l, r) => {
            leaves(p, l, out);
            leaves(p, r, out);
        }
    }
}

fn main() {
    // Caller-side setup: inputs and destinations allocated BEFORE measuring.
    let big: Vec<i64> = vec![i64::MAX / 2, i64::MAX / 2, 10];
    let small: Vec<i64> = (1..=100).collect();
    let xs32: Vec<i32> = vec![i32::MIN, -1, 0, 7, i32::MAX];
    let digits = b"9071";
    let notdigits = b"90x1";
    let ne = NonEmpty::new(&[i32::MAX, i32::MAX, i32::MAX - 3]).unwrap();
    let ne2 = NonEmpty::new(&[5, 1, 9, 3, 7]).unwrap();
    assert!(NonEmpty::new(&[]).is_none());

    let mut src = Pool::exact(15);
    let mut next = 0;
    let t = build(&mut src, 3, &mut next);

    // Destinations: zero values, sized by the header bounds.
    let mut d_sum = 0i64;
    let mut d_dbl = vec![0i64; xs32.len()]; // size out == size xs
    let mut d_dig = vec![0u8; digits.len()]; // size out <= size s
    let mut d_tree = Pool::exact(src.len()); // size out == size t
    let mut d_avg = 0i32;
    let mut d_med = 0i32;

    println!("dps (ordinary fns):");
    let r = measure("sum(small)", || dps::sum(&small, &mut d_sum));
    println!("    -> {r:?} {d_sum}");
    let r = measure("sum(big)", || dps::sum(&big, &mut d_sum));
    println!("    -> {r:?}");
    measure("doubled", || dps::doubled(&xs32, &mut d_dbl));
    println!("    -> {d_dbl:?}");
    let r = measure("parse_digits", || dps::parse_digits(digits, &mut d_dig));
    println!("    -> {r:?} {d_dig:?}");
    let r = measure("parse_digits!", || dps::parse_digits(notdigits, &mut d_dig));
    println!("    -> {r:?}");
    let m = measure("mirror", || dps::mirror(&src, t, &mut d_tree));
    let (mut before, mut after) = (Vec::new(), Vec::new());
    leaves(&src, t, &mut before);
    leaves(&d_tree, m, &mut after);
    println!("    -> leaves {before:?} -> {after:?} (dst len={} cap={})", d_tree.len(), d_tree.capacity());
    measure("average", || dps::average(ne, &mut d_avg));
    println!("    -> {d_avg}");
    measure("median", || dps::median(ne2, &mut d_med));
    println!("    -> {d_med}");
    alx_rt::report("06");

    println!("konst (const fn):");
    let mut k_sum = 0i64;
    let mut k_dbl = vec![0i64; xs32.len()];
    let mut k_dig = vec![0u8; digits.len()];
    let mut k_avg = 0i32;
    assert_eq!(konst::sum(&small, &mut k_sum), Ok(()));
    assert_eq!(k_sum, 5050);
    assert_eq!(konst::sum(&big, &mut k_sum), Err(PureError::Overflow));
    konst::doubled(&xs32, &mut k_dbl);
    assert_eq!(k_dbl, d_dbl);
    assert_eq!(konst::parse_digits(digits, &mut k_dig), Ok(4));
    assert_eq!(k_dig, d_dig);
    assert_eq!(konst::parse_digits(notdigits, &mut k_dig), Err(PureError::BadDigit(b'x')));
    konst::average(ne, &mut k_avg);
    assert_eq!(k_avg, d_avg);
    println!("  agrees with dps on sum, doubled, parse_digits, average");
    println!("  COMPILE_TIME = {:?}", konst::COMPILE_TIME);
    std::panic::set_hook(Box::new(|_| {}));
    let p = std::panic::catch_unwind(|| konst::panics(&[1, 2, 3]));
    println!("  konst::panics compiled; at runtime -> {}", if p.is_err() { "PANIC" } else { "ok" });
}

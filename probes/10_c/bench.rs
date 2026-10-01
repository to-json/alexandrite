//! Probe 10: the same workloads as bench.c, as the Rust backend would emit
//! them. Single file, std only, so rustc and clang compile comparable inputs.
use std::time::Instant;

const N: usize = 10_000_000;
const DEPTH: u32 = 20;
const REPS: usize = 5;

#[derive(Debug)]
enum PureError {
    Overflow,
}

fn sum(xs: &[i64], out: &mut i64) -> Result<(), PureError> {
    let mut acc: i64 = 0;
    for &x in xs {
        acc = acc.checked_add(x).ok_or(PureError::Overflow)?;
    }
    *out = acc;
    Ok(())
}

fn doubled(xs: &[i32], out: &mut [i64]) {
    for (o, &x) in out.iter_mut().zip(xs) {
        *o = x as i64 * 2;
    }
}

fn select_map(xs: &[i64], out: &mut Vec<i64>) {
    for &it in xs {
        if it % 3 != 0 {
            continue;
        }
        out.push(it * 3);
    }
}

fn parse_digits(s: &[u8], out: &mut [u8]) -> Result<(), u8> {
    for (o, &b) in out.iter_mut().zip(s) {
        if !b.is_ascii_digit() {
            return Err(b);
        }
        *o = b - b'0';
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Tree {
    Leaf(i64),
    Node(u32, u32),
}

struct TreePool {
    slots: Vec<Tree>,
}

impl TreePool {
    fn put(&mut self, t: Tree) -> u32 {
        assert!(self.slots.len() < self.slots.capacity(), "pool exceeded its header bound");
        self.slots.push(t);
        self.slots.len() as u32 - 1
    }
}

fn build(p: &mut TreePool, depth: u32, next: &mut i64) -> u32 {
    if depth == 0 {
        *next += 1;
        return p.put(Tree::Leaf(*next));
    }
    let l = build(p, depth - 1, next);
    let r = build(p, depth - 1, next);
    p.put(Tree::Node(l, r))
}

fn mirror(src: &TreePool, t: u32, dst: &mut TreePool) -> u32 {
    match src.slots[t as usize] {
        Tree::Leaf(n) => dst.put(Tree::Leaf(n)),
        Tree::Node(l, r) => {
            let r2 = mirror(src, r, dst);
            let l2 = mirror(src, l, dst);
            dst.put(Tree::Node(r2, l2))
        }
    }
}

fn leaves_hash(p: &TreePool, t: u32, h: &mut u64) {
    match p.slots[t as usize] {
        Tree::Leaf(n) => *h = h.wrapping_mul(31).wrapping_add(n as u64),
        Tree::Node(l, r) => {
            leaves_hash(p, l, h);
            leaves_hash(p, r, h);
        }
    }
}

fn gather(xs: &[i64], out: &mut [i64]) {
    let n = xs.len();
    for (i, o) in out.iter_mut().enumerate() {
        *o = xs[(i * 7919) % n];
    }
}

fn hash_i64(xs: &[i64]) -> u64 {
    xs.iter().fold(0u64, |h, &x| h.wrapping_mul(31).wrapping_add(x as u64))
}

fn time(label: &str, mut f: impl FnMut()) {
    let mut best = f64::MAX;
    for _ in 0..REPS {
        let t0 = Instant::now();
        f();
        best = best.min(t0.elapsed().as_secs_f64() * 1e3);
    }
    print!("{label:<12} {best:8.2} ms  ");
}

fn main() {
    let xs: Vec<i64> = (0..N).map(|i| (i * 7 % 1000) as i64).collect();
    let x32: Vec<i32> = (0..N).map(|i| (i as u64).wrapping_mul(2654435761) as i32).collect();
    let digits: Vec<u8> = (0..N).map(|i| b'0' + (i % 10) as u8).collect();

    let mut s = 0;
    time("sum", || sum(&xs, &mut s).unwrap());
    println!("checksum {s}");

    let mut d = vec![0i64; N];
    time("doubled", || doubled(&x32, &mut d));
    println!("checksum {}", hash_i64(&d));

    let mut sm = Vec::with_capacity(N);
    time("select_map", || {
        sm.clear();
        select_map(&xs, &mut sm)
    });
    println!("checksum {}", hash_i64(&sm));

    let mut pd = vec![0u8; N];
    let mut ok = 0;
    time("parse_digits", || ok = parse_digits(&digits, &mut pd).is_ok() as i32);
    let ph = pd.iter().fold(0u64, |h, &b| h.wrapping_mul(31).wrapping_add(b as u64));
    println!("checksum {ph} ok={ok}");

    let nodes = (1usize << (DEPTH + 1)) - 1;
    let mut src = TreePool { slots: Vec::with_capacity(nodes) };
    let mut next = 0;
    let root = build(&mut src, DEPTH, &mut next);
    let mut dst = TreePool { slots: Vec::with_capacity(nodes) };
    let mut m = 0;
    time("mirror", || {
        dst.slots.clear();
        m = mirror(&src, root, &mut dst)
    });
    let mut th = 0u64;
    leaves_hash(&dst, m, &mut th);
    println!("checksum {th}");

    let mut g = vec![0i64; N];
    time("gather", || gather(&xs, &mut g));
    println!("checksum {}", hash_i64(&g));
}

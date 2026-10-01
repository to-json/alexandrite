//! Probe 08, hand-emitted. CSP on tokio with inferred async.
//!
//! Async inference (nothing in src.alx is marked):
//!   fn          async?  why
//!   parse       no      #[pure]: no I/O, no channels, so it can never suspend
//!   fetch       yes     sleep (stands in for I/O)
//!   producer    yes     calls fetch; channel send
//!   parser      yes     channel recv + send; calls parse (sync)
//!   tally       yes     channel recv
//!   collatz     no      #[pure]
//!   leaves      no      gen block (a coroutine, but synchronous)
//!
//! Expected output:
//!   ann 1350 (10), bo 1450 (10), cy 1550 (10)
//!   pmap == sequential map
//!   concurrently: max=1550 min=1350
//!   leaves.find { > 5 } = Some(6), visiting 6 leaves (lazy)
#![feature(gen_blocks)]

use alx_rt::{Id, Pool};
use std::cell::Cell;
use std::time::Duration;
use tokio::sync::mpsc;

const CUSTOMERS: [&str; 3] = ["ann", "bo", "cy"];

#[derive(Debug)]
pub struct Order {
    pub id: i64,
    pub customer: String,
    pub total: i64,
}

#[derive(Debug)]
pub struct ParseError(pub String);

/// #[pure]: sync. (Not const: it builds a String; see probe 07.)
fn parse(line: &str) -> Result<Order, ParseError> {
    let bad = || ParseError(line.to_owned());
    let mut f = line.split(',').map(str::trim);
    let id = f.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let customer = f.next().map(str::to_owned).ok_or_else(bad)?;
    let total = f.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    Ok(Order { id, customer, total })
}

async fn fetch(i: i64) -> String {
    tokio::time::sleep(Duration::from_millis(1)).await;
    format!("{i}, {}, {}", CUSTOMERS[(i % 3) as usize], i * 10)
}

/// `out << x` on a closed channel ends the sender quietly.
async fn producer(n: i64, out: mpsc::Sender<String>) {
    for i in 0..n {
        if out.send(fetch(i).await).await.is_err() {
            return;
        }
    }
}

/// `inp.each { … }` over a channel: inlined into a recv loop (probe 05 rule).
/// `try` inside the block leaves `parser`.
async fn parser(mut inp: mpsc::Receiver<String>, out: mpsc::Sender<Order>) -> Result<(), ParseError> {
    while let Some(line) = inp.recv().await {
        if out.send(parse(&line)?).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

pub struct Total {
    customer: String,
    sum: i64,
    count: u32,
}

/// A pool that moves as a unit. Returning it from a task is a send.
pub struct Totals {
    pool: Pool<Total>,
}

impl Totals {
    fn add(&mut self, customer: &str, total: i64) {
        if let Some(t) = self.pool.iter_mut().find(|t| t.customer == customer) {
            t.sum += total;
            t.count += 1;
            return;
        }
        self.pool.put(Total { customer: customer.to_owned(), sum: total, count: 1 });
    }
    fn report(&self) -> String {
        self.pool.iter().map(|t| format!("{} {} ({})", t.customer, t.sum, t.count)).collect::<Vec<_>>().join(", ")
    }
    async fn max(&self) -> i64 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        self.pool.iter().map(|t| t.sum).max().unwrap_or(0)
    }
    async fn min(&self) -> i64 {
        tokio::time::sleep(Duration::from_millis(1)).await;
        self.pool.iter().map(|t| t.sum).min().unwrap_or(0)
    }
}

async fn tally(mut inp: mpsc::Receiver<Order>) -> Totals {
    let mut totals = Totals { pool: Pool::with_headroom(3) };
    while let Some(o) = inp.recv().await {
        totals.add(&o.customer, o.total);
    }
    totals
}

/// #[pure]
fn collatz(mut n: u64) -> u32 {
    let mut steps = 0;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        steps += 1;
    }
    steps
}

/// `pmap` over a pure block: scoped threads (CPU work, not tasks), each
/// filling its own slice of a pre-sized destination. Borrowing `xs` is fine
/// because the scope joins before returning.
fn pmap<T: Sync, R: Send + Default + Clone>(xs: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let mut out = vec![R::default(); xs.len()];
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = xs.len().div_ceil(workers).max(1);
    std::thread::scope(|s| {
        for (src, dst) in xs.chunks(chunk).zip(out.chunks_mut(chunk)) {
            let f = &f;
            s.spawn(move || {
                for (x, o) in src.iter().zip(dst) {
                    *o = f(x);
                }
            });
        }
    });
    out
}

#[derive(Clone, Copy)]
enum Tree {
    Leaf(i64),
    Node(Id<Tree>, Id<Tree>),
}

/// Recursive `each`: can't be inlined, so it's a gen block. Each level boxes
/// its iterator: O(depth) indirection per element.
fn leaves<'a>(p: &'a Pool<Tree>, t: Id<Tree>, visits: &'a Cell<u32>) -> Box<dyn Iterator<Item = i64> + 'a> {
    Box::new(gen move {
        match *p.get(t) {
            Tree::Leaf(n) => {
                visits.set(visits.get() + 1);
                yield n;
            }
            Tree::Node(l, r) => {
                for x in leaves(p, l, visits) {
                    yield x;
                }
                for x in leaves(p, r, visits) {
                    yield x;
                }
            }
        }
    })
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

/// `find` inlined over the external iterator (probe 05 rule): `return` from
/// the block is a plain return.
fn find_gt(p: &Pool<Tree>, t: Id<Tree>, k: i64, visits: &Cell<u32>) -> Option<i64> {
    for it in leaves(p, t, visits) {
        if it > k {
            return Some(it);
        }
    }
    None
}

#[tokio::main]
async fn main() {
    // --- pipeline: three tasks, channels between them
    let (lines_tx, lines_rx) = mpsc::channel(16);
    let (orders_tx, orders_rx) = mpsc::channel(16);
    tokio::spawn(producer(30, lines_tx));
    let p = tokio::spawn(parser(lines_rx, orders_tx));
    let t = tokio::spawn(tally(orders_rx)).await.unwrap(); // the pool moves here
    p.await.unwrap().unwrap();
    println!("tally: {}", t.report());

    // --- pmap: pure, CPU-parallel
    let xs: Vec<u64> = (1..=8).map(|x| x * 1000).collect();
    let par = pmap(&xs, |&x| collatz(x));
    let seq: Vec<u32> = xs.iter().map(|&x| collatz(x)).collect();
    assert_eq!(par, seq);
    println!("pmap collatz: {par:?} (== sequential)");

    // --- concurrently: two futures borrow `t`, no spawn, no 'static needed
    let (a, b) = tokio::join!(t.max(), t.min());
    println!("concurrently: max={a} min={b}");

    // --- recursive iterator via gen block, lazily stopped by `find`
    let mut tp = Pool::with_headroom(15);
    let mut next = 0;
    let root = build(&mut tp, 3, &mut next);
    let visits = Cell::new(0);
    let found = find_gt(&tp, root, 5, &visits);
    println!("leaves.find {{ it > 5 }} = {found:?}, visited {} of 8 leaves", visits.get());

    // --- an UNBRANDED Id crosses a task boundary: rustc accepts it.
    // Only the front end can reject this (the handle means nothing without
    // its pool).
    let mut cities: Pool<String> = Pool::with_headroom(1);
    let id = cities.put("Ashby".into());
    let leaked = tokio::spawn(async move { id }).await.unwrap();
    println!("unbranded Id crossed a spawn: {leaked:?} (rustc allowed it)");

    #[cfg(feature = "handle_send")]
    alx_rt::with_pool(4, |mut cities: alx_rt::BPool<'_, String>| {
        let h = cities.put("Ashby".into());
        tokio::spawn(async move {
            let _h = h;
        });
    });

    #[cfg(feature = "scoped_spawn")]
    {
        let h = tokio::spawn(async { t.max().await });
        h.await.unwrap();
    }
}

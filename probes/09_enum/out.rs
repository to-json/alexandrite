//! Probe 09, hand-emitted. Enumerable compilation decisions.
//!
//! Expected:
//!   1. impure map+take: materialized prints 6 lines (Ruby semantics); a
//!      fused emission would print 3 (wrong). Pure map+take: fused, same
//!      result, 3 calls instead of 6.
//!   2. size-rule destinations: 1 allocation and no growth for select.map
//!      and select.take; naive `collect` reallocates repeatedly.
//!      flat_map x2 fits 2x headroom; flat_map x3 spills.
//!   3. ownership modes compile as iter / iter_mut / into_iter;
//!      `--features use_after_consume` fails with E0382.

use alx_rt::Counting;
use std::cell::Cell;

#[global_allocator]
static A: Counting = Counting;

/// Run `f`, report allocations it made and the capacity of its result.
fn measure<T>(label: &str, f: impl FnOnce() -> Vec<T>) -> Vec<T> {
    let a0 = alx_rt::alloc_count();
    let v = f();
    let allocs = alx_rt::alloc_count() - a0;
    let waste = (v.capacity() - v.len()) * size_of::<T>();
    println!("  {label:<34} len={:<6} cap={:<6} allocs={allocs:<3} unused_bytes={waste}", v.len(), v.capacity());
    v
}

// ---------- 1. fusion vs materialization ----------

/// `xs.map { puts …; it * 2 }.take(3)`, impure block: materialize each stage.
fn impure_materialized(xs: &[i64], out: &mut Vec<String>) -> Vec<i64> {
    let mut stage1 = Vec::with_capacity(xs.len()); // map: == size
    for &it in xs {
        out.push(format!("map {it}"));
        stage1.push(it * 2);
    }
    let mut stage2 = Vec::with_capacity(3.min(stage1.len())); // take: <= min(3, size)
    for &it in stage1.iter().take(3) {
        stage2.push(it);
    }
    stage2
}

/// What a fusing emitter would produce for the same impure chain: WRONG
/// without `lazy`. With `xs.lazy.map { … }.take(3)` this is exactly the
/// requested emission: element-at-a-time, effects interleaved, stops early.
fn impure_fused(xs: &[i64], out: &mut Vec<String>) -> Vec<i64> {
    let mut r = Vec::with_capacity(3.min(xs.len()));
    for &it in xs {
        out.push(format!("map {it}"));
        r.push(it * 2);
        if r.len() == 3 {
            break;
        }
    }
    r
}

/// `xs.map { it * 2 }.take(3)`, pure block: fuse. `calls` is emitter
/// instrumentation, not user code.
fn pure_fused(xs: &[i64], calls: &Cell<u32>) -> Vec<i64> {
    let mut r = Vec::with_capacity(3.min(xs.len()));
    for &it in xs {
        calls.set(calls.get() + 1);
        r.push(it * 2);
        if r.len() == 3 {
            break;
        }
    }
    r
}

fn pure_materialized(xs: &[i64], calls: &Cell<u32>) -> Vec<i64> {
    let mut stage1 = Vec::with_capacity(xs.len());
    for &it in xs {
        calls.set(calls.get() + 1);
        stage1.push(it * 2);
    }
    stage1.into_iter().take(3).collect()
}

// ---------- 3. ownership modes ----------

#[derive(Debug)]
struct Order {
    total: i64,
}

fn ownership() {
    let mut orders = vec![Order { total: 10 }, Order { total: 20 }, Order { total: 30 }];
    let mut archive = Vec::with_capacity(orders.len()); // `<<` of every element: == size

    // orders.sum(&:total): block only reads -> iter()
    let mut sum = 0;
    for it in orders.iter() {
        sum += it.total;
    }

    // orders.each { it.total += 1 }: block mutates -> iter_mut()
    for it in orders.iter_mut() {
        it.total += 1;
    }

    // orders.each { archive << it }: block consumes -> into_iter()
    for it in orders.into_iter() {
        archive.push(it);
    }

    #[cfg(feature = "use_after_consume")]
    println!("{}", orders.len()); // puts orders.size

    println!("  sum={sum} archive={archive:?}");
}

fn main() {
    println!("1. fusion");
    let xs: Vec<i64> = (1..=6).collect();
    let (mut log_m, mut log_f) = (Vec::new(), Vec::new());
    let rm = impure_materialized(&xs, &mut log_m);
    let rf = impure_fused(&xs, &mut log_f);
    assert_eq!(rm, rf);
    println!("  impure, materialized: result={rm:?} printed {} lines (Ruby semantics)", log_m.len());
    println!("  impure, fused:        result={rf:?} printed {} lines  <- observable difference", log_f.len());
    println!("  impure, .lazy:        same emission as fused: {:?}", log_f);
    let (cf, cm) = (Cell::new(0), Cell::new(0));
    let pf = pure_fused(&xs, &cf);
    let pm = pure_materialized(&xs, &cm);
    assert_eq!(pf, pm);
    println!("  pure, fused:          result={pf:?} block calls={}", cf.get());
    println!("  pure, materialized:   result={pm:?} block calls={}", cm.get());

    println!("2. size rules (n=100000; input allocated before measuring)");
    let big: Vec<i64> = (0..100_000).collect();

    measure("select.map  naive collect", || big.iter().filter(|&&x| x % 3 == 0).map(|&x| x * 3).collect());
    measure("select.map  size rule (<= size)", || {
        let mut out = Vec::with_capacity(big.len());
        for &it in &big {
            if it % 3 == 0 {
                out.push(it * 3);
            }
        }
        out
    });
    measure("select.map  size rule + shrink", || {
        let mut out = Vec::with_capacity(big.len());
        for &it in &big {
            if it % 3 == 0 {
                out.push(it * 3);
            }
        }
        out.shrink_to_fit();
        out
    });
    measure("select.take(10) naive collect", || big.iter().filter(|&&x| x % 3 == 0).take(10).copied().collect());
    measure("select.take(10) size rule", || {
        let mut out = Vec::with_capacity(10.min(big.len()));
        for &it in &big {
            if it % 3 != 0 {
                continue;
            }
            out.push(it);
            if out.len() == 10 {
                break;
            }
        }
        out
    });
    measure("flat_map x2 naive collect", || big.iter().flat_map(|&x| [x, x]).collect());
    measure("flat_map x2 2x headroom", || {
        let mut out = Vec::with_capacity(2 * big.len());
        for &it in &big {
            out.extend([it, it]);
        }
        out
    });
    measure("flat_map x3 2x headroom (spills)", || {
        let mut out = Vec::with_capacity(2 * big.len());
        for &it in &big {
            out.extend([it, it, it]);
        }
        out
    });

    println!("3. ownership modes");
    ownership();
}

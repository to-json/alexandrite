//! Probe 02, hand-emitted. Region leaks in loops.
//! usage: probe_lines <N> [leaky]
//!   default: per-iteration scratch pool, survivors copied to the outer pool
//!   leaky:   everything allocated into the outer pool (naive region inference)

use alx_rt::{Bytes, Counting, Span};
use std::fmt::Write;

#[global_allocator]
static A: Counting = Counting;

/// Not known at compile time: lines are dynamic. Estimate from the literal
/// skeleton; the spill counter tells us if the estimate is wrong.
const LINE_GUESS: usize = 32;

fn run(n: usize, leaky: bool) -> (usize, String) {
    let mut keep_bytes = Bytes::with_headroom(n * LINE_GUESS / 4);
    let mut keep: Vec<Span> = Vec::with_capacity(2 * n / 4);
    let mut scratch = Bytes::with_headroom(LINE_GUESS * 3);
    let mut line_buf = String::with_capacity(2 * LINE_GUESS);

    for i in 0..n {
        line_buf.clear();
        write!(line_buf, "   line {i} alpha {}   ", i * 7 % 100).unwrap();

        // Which region the block's temporaries live in.
        let pool: &mut Bytes = if leaky { &mut keep_bytes } else { &mut scratch };
        let raw = pool.push_str(&line_buf);
        let s = pool.trim(raw);
        let s = pool.map(s, |b| b.to_ascii_uppercase());
        let t = pool.map(s, |b| if b == b'A' { b'4' } else { b });

        if pool.get(t).contains('7') {
            let kept = if leaky { t } else { keep_bytes.copy_from(&scratch, t) };
            keep.push(kept);
        }
        if !leaky {
            scratch.clear();
        }
    }
    let last = keep.last().map(|s| keep_bytes.get(*s).to_owned()).unwrap_or_default();
    (keep.len(), last)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().map(|a| a.parse().unwrap()).unwrap_or(10);
    let leaky = args.next().as_deref() == Some("leaky");

    let base = alx_rt::live_bytes();
    let (count, last) = run(n, leaky);
    let peak = alx_rt::peak_bytes() - base;

    println!("n={n} mode={}", if leaky { "leaky" } else { "per-iter" });
    println!("kept={count} last={last:?}");
    println!("peak_bytes={peak} bytes_per_line={:.1}", peak as f64 / n as f64);
    alx_rt::report("02");
}

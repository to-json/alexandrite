// Reference for pe014: single-threaded, checked arithmetic like the
// Alexandrite version's `try`, rustc -O.
fn collatz_len(mut n: u64) -> Option<u64> {
    let mut steps = 1;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { n.checked_mul(3)?.checked_add(1)? };
        steps += 1;
    }
    Some(steps)
}
fn main() {
    let mut best = (0, 0);
    for n in 1..1_000_000u64 {
        let l = collatz_len(n).unwrap();
        if l > best.0 {
            best = (l, n);
        }
    }
    println!("{}", best.1);
}

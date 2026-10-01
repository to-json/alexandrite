// Reference for pe007: same algorithm (trial division by found primes up
// to sqrt), idiomatic Rust, rustc -O.
fn main() {
    let mut found: Vec<u64> = Vec::new();
    let mut n = 2u64;
    loop {
        if found.iter().take_while(|&&p| p * p <= n).all(|&p| n % p != 0) {
            found.push(n);
            if found.len() == 10_001 {
                println!("{n}");
                return;
            }
        }
        n += 1;
    }
}

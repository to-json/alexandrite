// Reference for pe010: sieve of Eratosthenes, rustc -O.
fn main() {
    let limit = 2_000_000usize;
    let mut sieve = vec![true; limit];
    sieve[0] = false;
    sieve[1] = false;
    let mut i = 2;
    while i * i < limit {
        if sieve[i] {
            let mut j = i * i;
            while j < limit {
                sieve[j] = false;
                j += i;
            }
        }
        i += 1;
    }
    let s: u64 = (0..limit).filter(|&k| sieve[k]).map(|k| k as u64).sum();
    println!("{s}");
}

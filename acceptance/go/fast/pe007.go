package main

import "fmt"

// Trial division by the primes found so far, up to sqrt(n) (same algorithm as lib/primes.alx).
func main() {
	found := make([]int, 0, 10_001)
	for n := 2; ; n++ {
		prime := true
		for _, p := range found {
			if p*p > n {
				break
			}
			if n%p == 0 {
				prime = false
				break
			}
		}
		if prime {
			if found = append(found, n); len(found) == 10_001 {
				fmt.Println(n)
				return
			}
		}
	}
}

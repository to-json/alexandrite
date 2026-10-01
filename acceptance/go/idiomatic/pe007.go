package main

import (
	"fmt"
	"iter"
)

// An infinite generator of primes, by trial division.
func primes() iter.Seq[int] {
	return func(yield func(int) bool) {
		var found []int
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
				found = append(found, n)
				if !yield(n) {
					return
				}
			}
		}
	}
}

func main() {
	count := 0
	for p := range primes() {
		if count++; count == 10_001 {
			fmt.Println(p)
			return
		}
	}
}

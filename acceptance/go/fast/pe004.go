package main

import "fmt"

// Every product a*b, 100 <= a <= b <= 999, checked; no pruning (same work as pe004.alx).
func main() {
	best := 0
	for a := 100; a <= 999; a++ {
		for b := a; b <= 999; b++ {
			p := a * b
			r := 0 // numeric reversal: no string
			for m := p; m > 0; m /= 10 {
				r = r*10 + m%10
			}
			if r == p && p > best {
				best = p
			}
		}
	}
	fmt.Println(best)
}

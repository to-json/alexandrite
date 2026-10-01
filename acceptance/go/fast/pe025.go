package main

import (
	"fmt"
	"math/big"
)

// b.to_s.size < 1000  <=>  b < 10**999 (same predicate, no string per step)
func main() {
	limit := new(big.Int).Exp(big.NewInt(10), big.NewInt(999), nil)
	a, b, i := big.NewInt(1), big.NewInt(1), 2
	for b.Cmp(limit) < 0 {
		a.Add(a, b)
		a, b = b, a
		i++
	}
	fmt.Println(i)
}

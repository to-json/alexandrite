package main

import (
	"fmt"
	"math/big"
)

func main() {
	a, b, i := big.NewInt(1), big.NewInt(1), 2
	for len(b.String()) < 1000 {
		a.Add(a, b)
		a, b = b, a
		i++
	}
	fmt.Println(i)
}

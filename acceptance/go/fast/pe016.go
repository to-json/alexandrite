package main

import (
	"fmt"
	"math/big"
)

func main() {
	n := new(big.Int).Exp(big.NewInt(2), big.NewInt(1000), nil)
	sum := 0
	for _, c := range n.String() {
		sum += int(c - '0')
	}
	fmt.Println(sum)
}

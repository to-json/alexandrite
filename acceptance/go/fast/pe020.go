package main

import (
	"fmt"
	"math/big"
)

func main() {
	f := big.NewInt(1)
	for i := int64(2); i <= 100; i++ {
		f.Mul(f, big.NewInt(i))
	}
	sum := 0
	for _, c := range f.String() {
		sum += int(c - '0')
	}
	fmt.Println(sum)
}

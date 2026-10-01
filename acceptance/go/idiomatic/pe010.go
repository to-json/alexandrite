package main

import (
	"fmt"
	"math"
)

func main() {
	const limit = 2_000_000
	composite := make([]bool, limit)
	composite[0], composite[1] = true, true
	for i := 2; i <= int(math.Sqrt(limit)); i++ {
		if composite[i] {
			continue
		}
		for j := i * i; j < limit; j += i {
			composite[j] = true
		}
	}
	sum := 0
	for i, c := range composite {
		if !c {
			sum += i
		}
	}
	fmt.Println(sum)
}

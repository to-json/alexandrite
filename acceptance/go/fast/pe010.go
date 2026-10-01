package main

import "fmt"

func main() {
	const limit = 2_000_000
	var composite [limit]bool // fixed size: bounds checks provably gone
	composite[0], composite[1] = true, true
	for i := 2; i*i < limit; i++ {
		if composite[i] {
			continue
		}
		for j := i * i; j < limit; j += i {
			composite[j] = true
		}
	}
	sum := 0
	for i := range limit {
		if !composite[i] {
			sum += i
		}
	}
	fmt.Println(sum)
}

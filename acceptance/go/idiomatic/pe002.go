package main

import (
	"fmt"
	"iter"
)

// An infinite generator, made finite by breaking out of the range.
func fibs() iter.Seq[int] {
	return func(yield func(int) bool) {
		a, b := 1, 2
		for yield(a) {
			a, b = b, a+b
		}
	}
}

func main() {
	sum := 0
	for f := range fibs() {
		if f > 4_000_000 {
			break
		}
		if f%2 == 0 {
			sum += f
		}
	}
	fmt.Println(sum)
}

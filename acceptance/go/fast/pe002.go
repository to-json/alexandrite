package main

import "fmt"

func main() {
	sum := 0
	for a, b := 1, 2; a <= 4_000_000; a, b = b, a+b {
		if a&1 == 0 {
			sum += a
		}
	}
	fmt.Println(sum)
}

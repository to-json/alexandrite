package main

import "fmt"

func main() {
	n := 100
	sum, squares := 0, 0
	for i := 1; i <= n; i++ {
		sum += i
		squares += i * i
	}
	fmt.Println(sum*sum - squares)
}

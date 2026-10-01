package main

import (
	"fmt"
	"strconv"
)

func isPalindrome(n int) bool {
	s := strconv.Itoa(n)
	for i, j := 0, len(s)-1; i < j; i, j = i+1, j-1 {
		if s[i] != s[j] {
			return false
		}
	}
	return true
}

func main() {
	best := 0
	for a := 100; a <= 999; a++ {
		for b := a; b <= 999; b++ {
			if p := a * b; isPalindrome(p) && p > best {
				best = p
			}
		}
	}
	fmt.Println(best)
}

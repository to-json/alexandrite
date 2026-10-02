package main

import (
	"cmp"
	"fmt"
	"slices"
	"strings"
)

func word(n int) string {
	const letters = "etaoinshrdlucmfwypvbgkjqxz"
	var b strings.Builder
	for x := n; ; {
		b.WriteByte(letters[x%26])
		x /= 26
		if x == 0 {
			break
		}
	}
	return b.String()
}

func main() {
	counts := map[string]int{}
	seed := 42
	for i := 0; i < 1_000_000; i++ {
		seed = (seed*1103515245 + 12345) % 2147483648
		counts[word(seed%20000)]++
	}
	type pair struct {
		w string
		n int
	}
	pairs := make([]pair, 0, len(counts))
	for w, n := range counts {
		pairs = append(pairs, pair{w, n})
	}
	slices.SortFunc(pairs, func(a, b pair) int {
		if a.n != b.n {
			return b.n - a.n
		}
		return cmp.Compare(a.w, b.w)
	})
	fmt.Println(len(counts))
	for _, p := range pairs[:5] {
		fmt.Println(p.w, p.n)
	}
}

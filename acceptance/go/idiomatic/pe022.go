package main

import (
	"fmt"
	"os"
	"slices"
	"strings"
)

func main() {
	data, err := os.ReadFile("fixtures/names.txt")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	names := strings.Split(strings.ReplaceAll(string(data), `"`, ""), ",")
	slices.Sort(names)
	total := 0
	for i, name := range names {
		score := 0
		for _, c := range []byte(name) {
			score += int(c) - 64
		}
		total += (i + 1) * score
	}
	fmt.Println(total)
}

package main

import (
	"fmt"
	"strings"
)

func main() {
	lines := make([]string, 0)
	for i := 0; i < 200_000; i++ {
		kind := "plain"
		if i%7 == 0 {
			kind = "lucky"
		}
		lines = append(lines, fmt.Sprintf("item %d: %d %s", i, i*i, kind))
	}
	text := strings.Join(lines, "\n")
	total := 0
	for _, line := range strings.Split(text, "\n") {
		for _, f := range strings.Split(line, " ") {
			total += len(f)
		}
	}
	fmt.Println(len(text))
	fmt.Println(total)
}

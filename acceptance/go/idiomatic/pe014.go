package main

import (
	"fmt"
	"runtime"
	"sync"
)

func collatzLen(n int) int {
	steps := 1
	for n != 1 {
		if n%2 == 0 {
			n /= 2
		} else {
			n = 3*n + 1
		}
		steps++
	}
	return steps
}

func main() {
	const limit = 1_000_000
	// Parallel map (the .alx uses pmap): one goroutine per CPU, a chunk each.
	lens := make([]int, limit)
	workers := runtime.NumCPU()
	chunk := (limit + workers - 1) / workers
	var wg sync.WaitGroup
	for w := 0; w < workers; w++ {
		lo, hi := w*chunk, min((w+1)*chunk, limit-1)
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := lo; i < hi; i++ {
				lens[i] = collatzLen(i + 1)
			}
		}()
	}
	wg.Wait()
	best := 0
	for i := range lens[:limit-1] {
		if lens[i] > lens[best] {
			best = i
		}
	}
	fmt.Println(best + 1)
}

package main

import (
	"fmt"
	"runtime"
	"sync"
)

// pmap -> one goroutine per CPU, interleaved starts for load balance, each
// keeping its own best: no million-element array.
func main() {
	const limit = 1_000_000
	workers := runtime.NumCPU()
	type best struct{ steps, n int }
	results := make([]best, workers)
	var wg sync.WaitGroup
	for w := range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			b := best{}
			for start := 1 + w; start < limit; start += workers {
				steps := 1
				for n := start; n != 1; steps++ {
					if n&1 == 0 {
						n >>= 1
					} else {
						n = 3*n + 1
					}
				}
				if steps > b.steps {
					b = best{steps, start}
				}
			}
			results[w] = b
		}()
	}
	wg.Wait()
	b := results[0]
	for _, r := range results[1:] {
		// smallest start among the longest chains, like max_by on the index list
		if r.steps > b.steps || (r.steps == b.steps && r.n < b.n) {
			b = r
		}
	}
	fmt.Println(b.n)
}

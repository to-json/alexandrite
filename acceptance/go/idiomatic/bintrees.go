package main

import "fmt"

// Binary trees (the Benchmarks Game program, max depth 14), with pointers.
type Node struct{ left, right *Node }

func make_(d int) *Node {
	if d == 0 {
		return &Node{}
	}
	return &Node{make_(d - 1), make_(d - 1)}
}

func (n *Node) check() int {
	if n.left == nil {
		return 1
	}
	return 1 + n.left.check() + n.right.check()
}

func main() {
	minDepth, maxDepth := 4, 14
	fmt.Printf("stretch tree of depth %d check: %d\n", maxDepth+1, make_(maxDepth+1).check())
	long := make_(maxDepth)
	for d := minDepth; d <= maxDepth; d += 2 {
		iters := 1 << (maxDepth - d + minDepth)
		total := 0
		for i := 0; i < iters; i++ {
			total += make_(d).check()
		}
		fmt.Printf("%d trees of depth %d check: %d\n", iters, d, total)
	}
	fmt.Printf("long lived tree of depth %d check: %d\n", maxDepth, long.check())
}

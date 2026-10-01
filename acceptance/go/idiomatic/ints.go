// The Go original of ints.alx: its output is the expected output.
package main

import "fmt"

const (
	big     = 1 << 100
	shifted = big >> 98
	third   = float64(1.0 / 3)
	kb      = uint32(1024)
)

type Pix struct{ r, g, b, a uint8 }

func main() {
	fmt.Println(0.1 + 0.2)
	fmt.Println(shifted)
	fmt.Println(third)
	fmt.Println(kb * 4)
	var a, b uint8 = 200, 55
	fmt.Println(a + b)
	fmt.Println(a + b + 1)
	fmt.Println(a - 201)
	fmt.Println(a * 2)
	var c int8 = -128
	fmt.Println(c - 1)
	x := 0xff_ff
	fmt.Println(x)
	fmt.Println(x & 0x0f0f)
	fmt.Println(x | 0x10000)
	fmt.Println(x ^ 0xffff)
	fmt.Println(x &^ 0xff)
	fmt.Println(^0)
	fmt.Println(^a)
	fmt.Println(1 << 62)
	n := 64
	fmt.Println(1 << n)
	fmt.Println(-1 >> n)
	fmt.Println(0b1011 >> 1)
	var u uint64 = 18_446_744_073_709_551_615
	fmt.Println(u)
	fmt.Println(u / 3)
	fmt.Println(u % 10)
	fmt.Println(u > 1)
	fmt.Println(u >> 60)
	fmt.Println(int64(u))
	fmt.Println(float64(u))
	var m int32 = -7
	fmt.Println(m / 2)
	fmt.Println(m % 2)
	fmt.Println(int64(m))
	fmt.Println(uint32(m))
	big300 := 300
	fmt.Println(uint8(big300))
	f := 3.7
	fmt.Println(int8(f))
	fmt.Println(fmt.Sprintf("%x %X %o %b %d %c", 255, 255, 8, 5, -42, 9731))
	fmt.Println(fmt.Sprintf("%x %d", -255, u))
	bytes := []uint8{1, 2, 250}
	bytes[1] = bytes[2] + 10
	fmt.Println(bytes[1])
	total := 0
	for _, v := range bytes {
		total += int(v)
	}
	fmt.Println(total)
	p := Pix{r: 255, a: 128}
	p.g = p.r - 1
	fmt.Println(fmt.Sprintf("%d %d %d %d", p.r, p.g, p.b, p.a))
	var h uint32 = 2166136261
	for _, ch := range []byte("go") {
		h = (h ^ uint32(ch)) * 16777619
	}
	fmt.Println(h)
}

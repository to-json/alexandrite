// The Computer Language Benchmarks Game: n-body (single-threaded),
// written the way a Go programmer would. 1,000,000 steps.
package main

import (
	"fmt"
	"math"
)

const (
	solarMass   = 4 * math.Pi * math.Pi
	daysPerYear = 365.24
)

type Body struct {
	x, y, z, vx, vy, vz, mass float64
}

func energy(bodies []Body) float64 {
	e := 0.0
	for i, b := range bodies {
		e += 0.5 * b.mass * (b.vx*b.vx + b.vy*b.vy + b.vz*b.vz)
		for _, b2 := range bodies[i+1:] {
			dx, dy, dz := b.x-b2.x, b.y-b2.y, b.z-b2.z
			e -= b.mass * b2.mass / math.Sqrt(dx*dx+dy*dy+dz*dz)
		}
	}
	return e
}

func advance(bodies []Body, dt float64) {
	for i := range bodies {
		bi := &bodies[i]
		for j := i + 1; j < len(bodies); j++ {
			bj := &bodies[j]
			dx, dy, dz := bi.x-bj.x, bi.y-bj.y, bi.z-bj.z
			d2 := dx*dx + dy*dy + dz*dz
			mag := dt / (d2 * math.Sqrt(d2))
			bi.vx -= dx * bj.mass * mag
			bi.vy -= dy * bj.mass * mag
			bi.vz -= dz * bj.mass * mag
			bj.vx += dx * bi.mass * mag
			bj.vy += dy * bi.mass * mag
			bj.vz += dz * bi.mass * mag
		}
	}
	for i := range bodies {
		b := &bodies[i]
		b.x += dt * b.vx
		b.y += dt * b.vy
		b.z += dt * b.vz
	}
}

func main() {
	bodies := []Body{
		{mass: solarMass}, // sun
		{ // jupiter
			x: 4.84143144246472090e+00, y: -1.16032004402742839e+00, z: -1.03622044471123109e-01,
			vx: 1.66007664274403694e-03 * daysPerYear, vy: 7.69901118419740425e-03 * daysPerYear, vz: -6.90460016972063023e-05 * daysPerYear,
			mass: 9.54791938424326609e-04 * solarMass,
		},
		{ // saturn
			x: 8.34336671824457987e+00, y: 4.12479856412430479e+00, z: -4.03523417114321381e-01,
			vx: -2.76742510726862411e-03 * daysPerYear, vy: 4.99852801234917238e-03 * daysPerYear, vz: 2.30417297573763929e-05 * daysPerYear,
			mass: 2.85885980666130812e-04 * solarMass,
		},
		{ // uranus
			x: 1.28943695621391310e+01, y: -1.51111514016986312e+01, z: -2.23307578892655734e-01,
			vx: 2.96460137564761618e-03 * daysPerYear, vy: 2.37847173959480950e-03 * daysPerYear, vz: -2.96589568540237556e-05 * daysPerYear,
			mass: 4.36624404335156298e-05 * solarMass,
		},
		{ // neptune
			x: 1.53796971148509165e+01, y: -2.59193146099879641e+01, z: 1.79258772950371181e-01,
			vx: 2.68067772490389322e-03 * daysPerYear, vy: 1.62824170038242295e-03 * daysPerYear, vz: -9.51592254519715870e-05 * daysPerYear,
			mass: 5.15138902046611451e-05 * solarMass,
		},
	}
	// Offset the sun's momentum so the system's total is zero.
	var px, py, pz float64
	for _, b := range bodies {
		px += b.vx * b.mass
		py += b.vy * b.mass
		pz += b.vz * b.mass
	}
	bodies[0].vx = -px / solarMass
	bodies[0].vy = -py / solarMass
	bodies[0].vz = -pz / solarMass

	fmt.Printf("%.9f\n", energy(bodies))
	for range 1_000_000 {
		advance(bodies, 0.01)
	}
	fmt.Printf("%.9f\n", energy(bodies))
}

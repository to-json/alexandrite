# The Computer Language Benchmarks Game: n-body (single-threaded), 1,000,000 steps.
SOLAR_MASS = 4 * Math::PI**2
DAYS_PER_YEAR = 365.24

Body = Struct.new(:x, :y, :z, :vx, :vy, :vz, :mass, keyword_init: true)

def body(x, y, z, vx, vy, vz, mass)
  Body.new(x: x, y: y, z: z, vx: vx * DAYS_PER_YEAR, vy: vy * DAYS_PER_YEAR, vz: vz * DAYS_PER_YEAR, mass: mass * SOLAR_MASS)
end

def energy(bodies)
  e = 0.0
  bodies.each_with_index do |b, i|
    e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz)
    bodies[(i + 1)..].each do |b2|
      dx = b.x - b2.x
      dy = b.y - b2.y
      dz = b.z - b2.z
      e -= b.mass * b2.mass / Math.sqrt(dx * dx + dy * dy + dz * dz)
    end
  end
  e
end

def advance(bodies, dt)
  bodies.each_with_index do |bi, i|
    bodies[(i + 1)..].each do |bj|
      dx = bi.x - bj.x
      dy = bi.y - bj.y
      dz = bi.z - bj.z
      d2 = dx * dx + dy * dy + dz * dz
      mag = dt / (d2 * Math.sqrt(d2))
      bi.vx -= dx * bj.mass * mag
      bi.vy -= dy * bj.mass * mag
      bi.vz -= dz * bj.mass * mag
      bj.vx += dx * bi.mass * mag
      bj.vy += dy * bi.mass * mag
      bj.vz += dz * bi.mass * mag
    end
  end
  bodies.each do |b|
    b.x += dt * b.vx
    b.y += dt * b.vy
    b.z += dt * b.vz
  end
end

bodies = [
  Body.new(x: 0.0, y: 0.0, z: 0.0, vx: 0.0, vy: 0.0, vz: 0.0, mass: SOLAR_MASS),
  body(4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01, 1.66007664274403694e-03, 7.69901118419740425e-03, -6.90460016972063023e-05, 9.54791938424326609e-04),
  body(8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01, -2.76742510726862411e-03, 4.99852801234917238e-03, 2.30417297573763929e-05, 2.85885980666130812e-04),
  body(1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01, 2.96460137564761618e-03, 2.37847173959480950e-03, -2.96589568540237556e-05, 4.36624404335156298e-05),
  body(1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01, 2.68067772490389322e-03, 1.62824170038242295e-03, -9.51592254519715870e-05, 5.15138902046611451e-05),
]

px = py = pz = 0.0
bodies.each do |b|
  px += b.vx * b.mass
  py += b.vy * b.mass
  pz += b.vz * b.mass
end
bodies[0].vx = -px / SOLAR_MASS
bodies[0].vy = -py / SOLAR_MASS
bodies[0].vz = -pz / SOLAR_MASS

puts format("%.9f", energy(bodies))
1_000_000.times { advance(bodies, 0.01) }
puts format("%.9f", energy(bodies))

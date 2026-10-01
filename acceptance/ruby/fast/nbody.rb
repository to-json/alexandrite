# n-body, same work as nbody.alx, hand-optimized for YJIT: parallel Float
# arrays, while loops, no allocation in the step.
SOLAR_MASS = 4 * Math::PI**2
DAYS_PER_YEAR = 365.24
N = 5
X = [0.0, 4.84143144246472090e+00, 8.34336671824457987e+00, 1.28943695621391310e+01, 1.53796971148509165e+01]
Y = [0.0, -1.16032004402742839e+00, 4.12479856412430479e+00, -1.51111514016986312e+01, -2.59193146099879641e+01]
Z = [0.0, -1.03622044471123109e-01, -4.03523417114321381e-01, -2.23307578892655734e-01, 1.79258772950371181e-01]
VX = [0.0, 1.66007664274403694e-03, -2.76742510726862411e-03, 2.96460137564761618e-03, 2.68067772490389322e-03].map { it * DAYS_PER_YEAR }
VY = [0.0, 7.69901118419740425e-03, 4.99852801234917238e-03, 2.37847173959480950e-03, 1.62824170038242295e-03].map { it * DAYS_PER_YEAR }
VZ = [0.0, -6.90460016972063023e-05, 2.30417297573763929e-05, -2.96589568540237556e-05, -9.51592254519715870e-05].map { it * DAYS_PER_YEAR }
M = [1.0, 9.54791938424326609e-04, 2.85885980666130812e-04, 4.36624404335156298e-05, 5.15138902046611451e-05].map { it * SOLAR_MASS }

def energy(x, y, z, vx, vy, vz, m)
  e = 0.0
  i = 0
  while i < N
    e += 0.5 * m[i] * (vx[i] * vx[i] + vy[i] * vy[i] + vz[i] * vz[i])
    j = i + 1
    while j < N
      dx = x[i] - x[j]
      dy = y[i] - y[j]
      dz = z[i] - z[j]
      e -= m[i] * m[j] / Math.sqrt(dx * dx + dy * dy + dz * dz)
      j += 1
    end
    i += 1
  end
  e
end

def advance(x, y, z, vx, vy, vz, m, dt)
  i = 0
  while i < N
    j = i + 1
    while j < N
      dx = x[i] - x[j]
      dy = y[i] - y[j]
      dz = z[i] - z[j]
      d2 = dx * dx + dy * dy + dz * dz
      mag = dt / (d2 * Math.sqrt(d2))
      vx[i] -= dx * m[j] * mag
      vy[i] -= dy * m[j] * mag
      vz[i] -= dz * m[j] * mag
      vx[j] += dx * m[i] * mag
      vy[j] += dy * m[i] * mag
      vz[j] += dz * m[i] * mag
      j += 1
    end
    i += 1
  end
  i = 0
  while i < N
    x[i] += dt * vx[i]
    y[i] += dt * vy[i]
    z[i] += dt * vz[i]
    i += 1
  end
end

px = py = pz = 0.0
N.times do |i|
  px += VX[i] * M[i]
  py += VY[i] * M[i]
  pz += VZ[i] * M[i]
end
VX[0] = -px / SOLAR_MASS
VY[0] = -py / SOLAR_MASS
VZ[0] = -pz / SOLAR_MASS

puts format("%.9f", energy(X, Y, Z, VX, VY, VZ, M))
k = 0
while k < 1_000_000
  advance(X, Y, Z, VX, VY, VZ, M, 0.01)
  k += 1
end
puts format("%.9f", energy(X, Y, Z, VX, VY, VZ, M))

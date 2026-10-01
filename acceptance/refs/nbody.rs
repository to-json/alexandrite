// Reference for nbody: single-threaded, plain arrays of structs, the same
// arithmetic in the same order as nbody.alx, rustc -O.
const PI: f64 = 3.141592653589793;
const SOLAR_MASS: f64 = 4.0 * PI * PI;
const DAYS_PER_YEAR: f64 = 365.24;
const N: usize = 5;

#[derive(Clone, Copy, Default)]
struct Body {
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
    mass: f64,
}

fn body(p: [f64; 3], v: [f64; 3], m: f64) -> Body {
    Body { x: p[0], y: p[1], z: p[2], vx: v[0] * DAYS_PER_YEAR, vy: v[1] * DAYS_PER_YEAR, vz: v[2] * DAYS_PER_YEAR, mass: m * SOLAR_MASS }
}

fn energy(b: &[Body; N]) -> f64 {
    let mut e = 0.0;
    for i in 0..N {
        e += 0.5 * b[i].mass * (b[i].vx * b[i].vx + b[i].vy * b[i].vy + b[i].vz * b[i].vz);
        for j in i + 1..N {
            let (dx, dy, dz) = (b[i].x - b[j].x, b[i].y - b[j].y, b[i].z - b[j].z);
            e -= b[i].mass * b[j].mass / (dx * dx + dy * dy + dz * dz).sqrt();
        }
    }
    e
}

fn advance(b: &mut [Body; N], dt: f64) {
    for i in 0..N {
        for j in i + 1..N {
            let (dx, dy, dz) = (b[i].x - b[j].x, b[i].y - b[j].y, b[i].z - b[j].z);
            let d2 = dx * dx + dy * dy + dz * dz;
            let mag = dt / (d2 * d2.sqrt());
            let (mi, mj) = (b[i].mass, b[j].mass);
            b[i].vx -= dx * mj * mag;
            b[i].vy -= dy * mj * mag;
            b[i].vz -= dz * mj * mag;
            b[j].vx += dx * mi * mag;
            b[j].vy += dy * mi * mag;
            b[j].vz += dz * mi * mag;
        }
    }
    for p in b.iter_mut() {
        p.x += dt * p.vx;
        p.y += dt * p.vy;
        p.z += dt * p.vz;
    }
}

fn main() {
    let mut b = [
        Body { mass: SOLAR_MASS, ..Default::default() },
        body([4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01], [1.66007664274403694e-03, 7.69901118419740425e-03, -6.90460016972063023e-05], 9.54791938424326609e-04),
        body([8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01], [-2.76742510726862411e-03, 4.99852801234917238e-03, 2.30417297573763929e-05], 2.85885980666130812e-04),
        body([1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01], [2.96460137564761618e-03, 2.37847173959480950e-03, -2.96589568540237556e-05], 4.36624404335156298e-05),
        body([1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01], [2.68067772490389322e-03, 1.62824170038242295e-03, -9.51592254519715870e-05], 5.15138902046611451e-05),
    ];
    let (mut px, mut py, mut pz) = (0.0, 0.0, 0.0);
    for p in &b {
        px += p.vx * p.mass;
        py += p.vy * p.mass;
        pz += p.vz * p.mass;
    }
    b[0].vx = -px / SOLAR_MASS;
    b[0].vy = -py / SOLAR_MASS;
    b[0].vz = -pz / SOLAR_MASS;
    println!("{:.9}", energy(&b));
    for _ in 0..1_000_000 {
        advance(&mut b, 0.01);
    }
    println!("{:.9}", energy(&b));
}

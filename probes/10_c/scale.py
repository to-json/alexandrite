#!/usr/bin/env python3
"""Probe 10 scaling: K renamed copies of the workload functions, all called
from main, so every copy is type-checked AND code-generated. Measures how
build time grows with program size."""
import re, statistics, subprocess, sys, time
from pathlib import Path

HERE = Path(__file__).parent
OUT = HERE.parent / "target" / "probe10"
OUT.mkdir(parents=True, exist_ok=True)
FNS = ["sum", "doubled", "select_map", "parse_digits", "mirror", "gather", "build", "leaves_hash"]

def body(src, start_marker, end_marker):
    return src[src.index(start_marker):src.index(end_marker)]

def gen_c(k):
    src = (HERE / "bench.c").read_text()
    head = src[:src.index("/* #[pure] def sum")]
    fns = body(src, "/* #[pure] def sum", "static uint64_t hash_i64")
    fns = fns.replace("typedef struct {\n    uint32_t tag;", "@@TREE@@")  # keep types once
    types = src[src.index("/* Tree in a pool"):src.index("static uint32_t tp_put")]
    tp_put = body(src, "static uint32_t tp_put", "static uint32_t build")
    fns = body(src, "/* #[pure] def sum", "/* Tree in a pool") + body(src, "static uint32_t build", "static uint64_t hash_i64")
    out = [head, types, tp_put]
    calls = []
    for i in range(k):
        f = fns
        for n in FNS:
            f = re.sub(rf"\b{n}\(", f"{n}_{i}(", f)
        out.append(f)
        calls.append(f"""    {{ int64_t xs[4] = {{1,2,3,4}}, o[4]; int32_t x3[4] = {{1,2,3,4}}; int64_t s;
      uint8_t dg[2] = {{'1','2'}}, od[2], bad; Tree sl[7], dl[7]; TreePool p = {{sl,0,7}}, q = {{dl,0,7}};
      int64_t nx = 0; uint64_t h = 0; VecI64 v = VecI64_with_capacity(4);
      acc += sum_{i}(xs, 4, &s); doubled_{i}(x3, 4, o); select_map_{i}(xs, 4, &v);
      acc += parse_digits_{i}(dg, 2, od, &bad); uint32_t r = build_{i}(&p, 2, &nx);
      leaves_hash_{i}(&q, mirror_{i}(&p, r, &q), &h); gather_{i}(xs, 4, o); acc += (long)h + o[0] + (long)v.len; VecI64_drop(&v); }}""")
    out.append("int main(void) {\n    long acc = 0;\n" + "\n".join(calls) + '\n    printf("%ld\\n", acc);\n    return 0;\n}\n')
    return "\n".join(out)

def gen_rs(k):
    src = (HERE / "bench.rs").read_text()
    head = src[:src.index("fn sum(")]
    fns = body(src, "fn sum(", "fn hash_i64")
    types = body(src, "#[derive(Clone, Copy)]\nenum Tree", "fn build(")
    fns = fns.replace(types, "")
    out = [head, types]
    calls = []
    for i in range(k):
        f = fns
        for n in FNS:
            f = re.sub(rf"\b{n}\(", f"{n}_{i}(", f)
        out.append(f)
        calls.append(f"""    {{ let xs = [1i64, 2, 3, 4]; let mut o = [0i64; 4]; let mut s = 0; let mut od = [0u8; 2];
      let mut v = Vec::with_capacity(4); let mut p = TreePool {{ slots: Vec::with_capacity(7) }};
      let mut q = TreePool {{ slots: Vec::with_capacity(7) }}; let mut nx = 0; let mut h = 0u64;
      acc += sum_{i}(&xs, &mut s).is_ok() as i64; doubled_{i}(&[1, 2, 3, 4], &mut o); select_map_{i}(&xs, &mut v);
      acc += parse_digits_{i}(b"12", &mut od).is_ok() as i64; let r = build_{i}(&mut p, 2, &mut nx);
      let m = mirror_{i}(&p, r, &mut q); leaves_hash_{i}(&q, m, &mut h); gather_{i}(&xs, &mut o);
      acc += h as i64 + o[0] + v.len() as i64; }}""")
    out.append("#[allow(unused)]\nfn main() {\n    let mut acc = 0i64;\n" + "\n".join(calls) + '\n    println!("{acc}");\n    let _ = (N, DEPTH, REPS, Instant::now(), PureError::Overflow);\n}\n')
    return "\n".join(out)

def timeit(cmd, runs=3):
    ts = []
    for _ in range(runs):
        t0 = time.perf_counter()
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode:
            sys.exit(f"FAILED: {' '.join(cmd)}\n{r.stderr[:2000]}")
        ts.append((time.perf_counter() - t0) * 1e3)
    return statistics.median(ts)

print("| copies | C lines | Rust lines | clang -O0 | clang -O2 | rustc check | rustc debug | rustc -O | outputs agree |")
print("|---|---|---|---|---|---|---|---|---|")
for k in (1, 10, 50, 200):
    c, rs = OUT / f"big{k}.c", OUT / f"big{k}.rs"
    c.write_text(gen_c(k)); rs.write_text(gen_rs(k))
    hdr = ["-I", str(HERE)]
    t = [
        timeit(["clang", "-std=c11", "-O0", "-fwrapv", *hdr, str(c), "-o", str(OUT / f"big{k}_c0")]),
        timeit(["clang", "-std=c11", "-O2", "-fwrapv", *hdr, str(c), "-o", str(OUT / f"big{k}_c2")]),
        timeit(["rustc", "--edition", "2024", "--emit=metadata", "-A", "warnings", str(rs), "-o", str(OUT / f"big{k}.rmeta")]),
        timeit(["rustc", "--edition", "2024", "-A", "warnings", str(rs), "-o", str(OUT / f"big{k}_rd")]),
        timeit(["rustc", "--edition", "2024", "-O", "-A", "warnings", str(rs), "-o", str(OUT / f"big{k}_rO")]),
    ]
    outs = {subprocess.run([str(OUT / b)], capture_output=True, text=True).stdout.strip() for b in (f"big{k}_c0", f"big{k}_c2", f"big{k}_rd", f"big{k}_rO")}
    lines = lambda p: len(p.read_text().splitlines())
    print(f"| {k} | {lines(c)} | {lines(rs)} | " + " | ".join(f"{x:.0f}" for x in t) + f" | {'yes' if len(outs) == 1 else outs} |")

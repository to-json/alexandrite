#!/usr/bin/env python3
"""Probe 13b scaling: compile time and binary size of monomorphized vs
dictionary-passing C for G generic functions x T types, every combination
called from main."""
import statistics, subprocess, time
from pathlib import Path

OUT = Path(__file__).parent.parent / "target" / "probe13"
OUT.mkdir(parents=True, exist_ok=True)

def types(T):
    out = []
    for j in range(T):
        out.append(f"typedef struct {{ double a, b; }} T{j};")
        out.append(f"static double T{j}_area(const T{j} *x) {{ return x->a * x->b + {j}; }}")
        out.append(f"static double T{j}_area_w(const void *p) {{ return T{j}_area(p); }}")
        out.append(f"static const WT T{j}_WT = {{ T{j}_area_w, sizeof(T{j}) }};")
    return out

BODY = """    double acc = 0;
    for (size_t k = 0; k < n; k++) {{
        double v = {area};
        if (v > {i}.5) acc += v * {i1}; else acc -= v / {i1};
        acc = acc > 1e9 ? acc / 2 : acc;
    }}
    return acc;"""

def gen(G, T, mode):
    src = ["#include <stddef.h>", "#include <stdio.h>",
           "typedef struct { double (*area)(const void *); size_t size; } WT;"]
    src += types(T)
    calls = []
    if mode == "mono":
        for i in range(G):
            for j in range(T):
                src.append(f"double g{i}_T{j}(const T{j} *xs, size_t n) {{\n" +
                           BODY.format(area=f"T{j}_area(&xs[k])", i=i, i1=i + 1) + "\n}")
                calls.append(f"acc += g{i}_T{j}((const T{j} *)buf, 4);")
    else:
        for i in range(G):
            src.append(f"double g{i}(const void *xs, size_t n, const WT *wt) {{\n" +
                       BODY.format(area="wt->area((const char *)xs + k * wt->size)", i=i, i1=i + 1) + "\n}")
            for j in range(T):
                calls.append(f"acc += g{i}(buf, 4, &T{j}_WT);")
    src.append("int main(void) {\n    double buf[8] = {1,2,3,4,5,6,7,8}, acc = 0;\n    " +
               "\n    ".join(calls) + '\n    printf("%.6f\\n", acc);\n    return 0;\n}')
    return "\n".join(src)

def timed(cmd, runs=3):
    ts = []
    for _ in range(runs):
        t0 = time.perf_counter()
        subprocess.run(cmd, check=True, capture_output=True)
        ts.append((time.perf_counter() - t0) * 1e3)
    return statistics.median(ts)

print("| G x T | form | C functions | clang -O0 ms | clang -O2 ms | -O2 binary KB | output |")
print("|---|---|---|---|---|---|---|")
for G, T in [(50, 10), (200, 20)]:
    outs = {}
    for mode in ("mono", "dict"):
        c = OUT / f"g{G}_t{T}_{mode}.c"
        c.write_text(gen(G, T, mode))
        o0 = timed(["clang", "-std=c11", "-O0", str(c), "-o", str(c.with_suffix(".o0"))])
        o2 = timed(["clang", "-std=c11", "-O2", str(c), "-o", str(c.with_suffix(".o2"))])
        size = c.with_suffix(".o2").stat().st_size / 1024
        outs[mode] = subprocess.run([str(c.with_suffix(".o2"))], capture_output=True, text=True).stdout.strip()
        nfn = G * T if mode == "mono" else G
        print(f"| {G}x{T} | {mode} | {nfn} | {o0:.0f} | {o2:.0f} | {size:.0f} | {outs[mode]} |")

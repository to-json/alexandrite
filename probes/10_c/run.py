#!/usr/bin/env python3
"""Probe 10 driver: build time, run time, checksums, sanitizers.

usage: run.py      (from anywhere; outputs go to probes/target/probe10)
"""

import statistics
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).parent
OUT = HERE.parent / "target" / "probe10"
OUT.mkdir(parents=True, exist_ok=True)
C, RS = str(HERE / "bench.c"), str(HERE / "bench.rs")
SAFE = ["-fwrapv", "-fstack-clash-protection"]
BUILD_RUNS = 5

BUILDS = {
    "clang -O0": ["clang", "-std=c11", "-O0", *SAFE, C, "-o", str(OUT / "c_O0")],
    "clang -O2": ["clang", "-std=c11", "-O2", *SAFE, C, "-o", str(OUT / "c_O2")],
    "clang -O1 +asan+ubsan": ["clang", "-std=c11", "-O1", "-g", *SAFE,
                              "-fsanitize=address,undefined", "-fno-sanitize-recover=all",
                              C, "-o", str(OUT / "c_san")],
    "rustc check (oracle)": ["rustc", "--edition", "2024", "--emit=metadata", RS,
                             "-o", str(OUT / "rs.rmeta")],
    "rustc debug": ["rustc", "--edition", "2024", RS, "-o", str(OUT / "rs_debug")],
    "rustc -O": ["rustc", "--edition", "2024", "-O", RS, "-o", str(OUT / "rs_O")],
}


def build_times():
    print("## build time (wall, ms; median of 5, min in parens)\n")
    print("| build | median | min |")
    print("|---|---|---|")
    for name, cmd in BUILDS.items():
        ts = []
        for _ in range(BUILD_RUNS):
            t0 = time.perf_counter()
            r = subprocess.run(cmd, capture_output=True, text=True)
            ts.append((time.perf_counter() - t0) * 1e3)
            if r.returncode:
                print(f"BUILD FAILED: {name}\n{r.stderr}")
                return False
            warn = [l for l in r.stderr.splitlines() if "warning" in l]
            if warn and _ == 0:
                print(f"<!-- {name}: {warn[0]} -->")
        print(f"| {name} | {statistics.median(ts):.0f} | {min(ts):.0f} |")
    print()
    return True


def run(binary, *args):
    r = subprocess.run([str(OUT / binary), *args], capture_output=True, text=True)
    return r.returncode, r.stdout, r.stderr


def parse(stdout):
    rows = {}
    for line in stdout.splitlines():
        parts = line.split()
        rows[parts[0]] = (float(parts[1]), " ".join(parts[3:]))
    return rows


def runtimes():
    print("## run time (ms, best of 5 reps inside the program)\n")
    results = {b: parse(run(b)[1]) for b in ("c_O2", "rs_O", "c_O0", "rs_debug")}
    print("| workload | C -O2 | Rust -O | C/Rust | C -O0 | Rust debug | checksums agree |")
    print("|---|---|---|---|---|---|---|")
    for w in results["c_O2"]:
        c, r = results["c_O2"][w], results["rs_O"][w]
        c0, r0 = results["c_O0"][w], results["rs_debug"][w]
        agree = len({c[1], r[1], c0[1], r0[1]}) == 1
        print(f"| {w} | {c[0]:.2f} | {r[0]:.2f} | {c[0] / r[0]:.2f} | {c0[0]:.2f} | {r0[0]:.2f} | {'yes' if agree else 'NO: ' + c[1] + ' vs ' + r[1]} |")
    print()


def sanitizers():
    print("## sanitizers (clang -fsanitize=address,undefined, -fno-sanitize-recover)\n")
    print("| run | exit | result |")
    print("|---|---|---|")
    code, out, err = run("c_san")
    reports = [l for l in err.splitlines() if "runtime error" in l or "AddressSanitizer" in l]
    print(f"| full workload | {code} | {'clean' if not reports and code == 0 else reports[:1]} |")
    code, out, err = run("c_san", "oob")
    msg = next((l for l in err.splitlines() if "alexandrite panic" in l), err.strip()[:80])
    print(f"| oob index | {code} | {msg} |")
    code, out, err = run("c_san", "overflow")
    reports = [l for l in err.splitlines() if "runtime error" in l]
    print(f"| overflow | {code} | {' / '.join(out.splitlines())}{' UB: ' + reports[0] if reports else ''} |")
    print()


if __name__ == "__main__":
    if build_times():
        runtimes()
        sanitizers()

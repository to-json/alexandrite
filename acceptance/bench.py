#!/usr/bin/env python3
"""Run and time the acceptance cases under any combination of runners.

usage: acceptance/bench.py [CASE ...] [-r RUNNERS] [--vs RUNNER] [-n N] [--once] [--list]

  CASE      case names or substrings (pe010, 01, ...); default: all
  -r        comma-separated runners or groups; default: all
  --vs      add a column per runner: its time / this runner's time
  --speedup add a column: fastest Ruby runner / alx-rel (implies alx-rel)
  -n        max runs per cell (best-of; default 50, capped at ~3 s per cell)
  --once    run each cell once and print its output instead of timing it
  --list    list cases and runners

Runners (wall ms from process start to exit; output checked each run):
  alx-run     `alx run`: debug build + run (edit-to-answer)
  alx-rel     prebuilt release binary (built before timing)
  rust        hand-written Rust reference, rustc -O (pe007, pe010, pe014)
  rb          ruby/idiomatic, `ruby`
  rb-yjit     ruby/idiomatic, `ruby --yjit`
  fast        ruby/fast, `ruby --disable-gems` (no JIT)
  fast-yjit   ruby/fast, --disable-gems --yjit --yjit-call-threshold=1
  fast-zjit   ruby/fast, --disable-gems --zjit --zjit-call-threshold=1
Groups: alx, ruby, idiomatic, fastrb, all.

ruby/idiomatic is a direct port. ruby/fast does the same work with the
same algorithm, hand-optimized (while loops, no intermediate arrays,
Ractors where the source uses pmap).

examples:
  acceptance/bench.py                          everything
  acceptance/bench.py pe010 pe014 -r alx-rel,fast-yjit,rust
  acceptance/bench.py -r alx,ruby --vs alx-rel
  acceptance/bench.py -r alx-rel,ruby --speedup
  acceptance/bench.py pe025 -r alx-rel,rb --once
"""

import argparse
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CASES = ROOT / "acceptance" / "cases"
REFS = ROOT / "acceptance" / "refs"
RB = ROOT / "acceptance" / "ruby"
ALX = ROOT / "target" / "release" / "alx"
WORK = Path(tempfile.mkdtemp(prefix="alx-bench-"))
CASE_NAMES = sorted(p.stem for p in CASES.glob("pe*.alx") if p.stem.count(".") == 0)

RUBY = shutil.which("ruby", path="/opt/homebrew/opt/ruby/bin") or "ruby"
FAST = [RUBY, "--disable-gems"]

# runner -> function(case) -> argv, or None if the runner has nothing for this case
RUNNERS = {
    "alx-run": lambda c: [ALX, "run", f"{c}.alx"],
    "alx-rel": lambda c: alx_release(c),
    "rust": lambda c: rust_ref(c),
    "rb": lambda c: [RUBY, RB / "idiomatic" / f"{c}.rb"],
    "rb-yjit": lambda c: [RUBY, "--yjit", RB / "idiomatic" / f"{c}.rb"],
    "fast": lambda c: FAST + [RB / "fast" / f"{c}.rb"],
    "fast-yjit": lambda c: FAST + ["--yjit", "--yjit-call-threshold=1", RB / "fast" / f"{c}.rb"],
    "fast-zjit": lambda c: FAST + ["--zjit", "--zjit-call-threshold=1", RB / "fast" / f"{c}.rb"],
}
GROUPS = {
    "alx": ["alx-run", "alx-rel"],
    "ruby": ["rb", "rb-yjit", "fast", "fast-yjit", "fast-zjit"],
    "idiomatic": ["rb", "rb-yjit"],
    "fastrb": ["fast", "fast-yjit", "fast-zjit"],
    "all": list(RUNNERS),
}


def alx_release(case):
    r = subprocess.run([ALX, "build", "--release", f"{case}.alx"], cwd=CASES, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"alx build --release {case}.alx failed:\n{r.stderr}")
    return [r.stdout.strip()]


def rust_ref(case):
    src = REFS / f"{case}.rs"
    if not src.exists():
        return None
    out = WORK / f"ref_{case}"
    if not out.exists():
        subprocess.run(["rustc", "--edition", "2024", "-O", src, "-o", out], check=True, capture_output=True)
    return [out]


def timed(cmd, want):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, cwd=CASES, capture_output=True, text=True)
    ms = (time.perf_counter() - t0) * 1e3
    ok = r.returncode == 0 and r.stdout.strip() == want
    return ms, ok, r


def best(cmd, want, nmax, budget_ms=3000, nmin=3):
    ts = []
    while len(ts) < nmax and (len(ts) < nmin or sum(ts) < budget_ms):
        ms, ok, r = timed(cmd, want)
        if not ok:
            return None, r
        ts.append(ms)
    return min(ts), None


def expand(spec):
    out = []
    for name in spec.split(","):
        names = GROUPS.get(name, [name])
        for n in names:
            if n not in RUNNERS:
                sys.exit(f"unknown runner {n!r}; try --list")
            if n not in out:
                out.append(n)
    return out


def main():
    ap = argparse.ArgumentParser(usage=__doc__.split("\n\n")[1].strip().removeprefix("usage: "))
    ap.add_argument("cases", nargs="*")
    ap.add_argument("-r", default="all")
    ap.add_argument("--vs")
    ap.add_argument("--speedup", action="store_true")
    ap.add_argument("-n", type=int, default=50)
    ap.add_argument("--once", action="store_true")
    ap.add_argument("--list", action="store_true")
    a = ap.parse_args()

    if a.list:
        print("cases:   " + " ".join(CASE_NAMES))
        print("runners: " + " ".join(RUNNERS))
        print("groups:  " + "  ".join(f"{g}={','.join(r)}" for g, r in GROUPS.items()))
        return

    cases = [c for c in CASE_NAMES if not a.cases or any(k in c for k in a.cases)]
    if not cases:
        sys.exit(f"no case matches {a.cases}; try --list")
    runners = expand(a.r)
    if a.vs:
        if a.vs not in RUNNERS:
            sys.exit(f"unknown runner {a.vs!r}")
        if a.vs not in runners:
            runners.append(a.vs)

    if a.speedup and "alx-rel" not in runners:
        runners.insert(0, "alx-rel")

    if any(r.startswith("alx") for r in runners):
        subprocess.run(["cargo", "build", "--release", "-q"], cwd=ROOT, check=True, stderr=subprocess.DEVNULL)
    if any(r in GROUPS["ruby"] for r in runners):
        print(subprocess.run([RUBY, "-v"], capture_output=True, text=True).stdout.strip())

    if a.once:
        for c in cases:
            want = (CASES / f"{c}.expected").read_text().strip()
            for r in runners:
                cmd = RUNNERS[r](c)
                if cmd is None:
                    continue
                ms, ok, res = timed(cmd, want)
                print(f"{c} {r:<10} {'ok  ' if ok else 'FAIL'} {ms:9.1f} ms  {res.stdout.strip()}")
                if res.stderr.strip():
                    print("    " + res.stderr.strip().replace("\n", "\n    "))
        return

    vs_cols = [r for r in runners if a.vs and r != a.vs]
    w = 11
    head = f"{'case':<7}" + "".join(f"{r:>{w}}" for r in runners) + "".join(f"{r + '/' + a.vs:>{max(w, len(r) + len(a.vs) + 3)}}" for r in vs_cols)
    if a.speedup:
        head += f"{'rb/alx-rel':>{w + 1}}"
    print(head)
    failed = False
    for c in cases:
        want = (CASES / f"{c}.expected").read_text().strip()
        row = {}
        for r in runners:
            cmd = RUNNERS[r](c)
            if cmd is None:
                row[r] = None
                continue
            ms, bad = best(cmd, want, a.n)
            if bad is not None:
                failed = True
                print(f"{c} {r}: wrong output {bad.stdout.strip()!r} {bad.stderr.strip()[:300]}", file=sys.stderr)
            row[r] = ms
        cells = [f"{row[r]:{w}.1f}" if row[r] is not None else f"{'-':>{w}}" for r in runners]
        for r in vs_cols:
            cw = max(w, len(r) + len(a.vs) + 3)
            ok = row[r] is not None and row[a.vs]
            cells.append(f"{row[r] / row[a.vs]:{cw - 1}.2f}x" if ok else f"{'-':>{cw}}")
        if a.speedup:
            rb = [row[r] for r in runners if r in GROUPS["ruby"] and row[r] is not None]
            cells.append(f"{min(rb) / row['alx-rel']:{w}.2f}x" if rb and row["alx-rel"] else f"{'-':>{w + 1}}")
        print(f"{c:<7}" + "".join(cells), flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

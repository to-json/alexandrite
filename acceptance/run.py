#!/usr/bin/env python3
"""Acceptance harness for ACCEPTANCE.md (A1–A6).

usage: acceptance/run.py [-k FILTER]

Builds the compiler (release), then for every case:
  1. debug   `alx run`: stdout, edit-to-answer budget
  2. release `alx build --release`: stdout, run time vs the Rust reference
  3. safety  ASan/UBSan build, and the Rust oracle (rustc must accept it,
             and its output must match)
  4. negative cases: the first error (message + file:line:col) must match
plus the A3 promote-overhead check, the A6 workflow checks and the runtime
license manifest. Exit status 0 iff everything passes.
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CASES = ROOT / "acceptance" / "cases"
REFS = ROOT / "acceptance" / "refs"
ALX = ROOT / "target" / "release" / "alx"
WORK = Path(tempfile.mkdtemp(prefix="alx-acceptance-"))

results = []  # (group, check, ok, detail)


def check(group, name, ok, detail=""):
    results.append((group, name, bool(ok), detail))
    mark = "PASS" if ok else "FAIL"
    print(f"  [{mark}] {group} {name}" + (f": {detail}" if detail else ""), flush=True)


def run(cmd, cwd=CASES, env=None, timeout=600):
    e = dict(os.environ)
    if env:
        e.update(env)
    t0 = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=e, timeout=timeout)
    return r, (time.perf_counter() - t0) * 1e3


def best_ms(cmd, n=11, cwd=CASES, env=None):
    return min(run(cmd, cwd=cwd, env=env)[1] for _ in range(n))


def budget(case):
    b = {}
    p = CASES / f"{case}.budget"
    if p.exists():
        for line in p.read_text().splitlines():
            if line.strip():
                k, v = line.split()
                b[k] = float(v)
    return b


GROUPS = {
    "pe001": "A1", "pe002": "A1", "pe006": "A1",
    "pe007": "A2", "pe010": "A2",
    "pe016": "A3", "pe020": "A3", "pe025": "A3",
    "pe004": "A4", "pe014": "A4",
    "pe008": "A5", "pe022": "A5",
    "nbody": "A7",
    "ints": "M1",
    "syntax": "M2",
    "collections": "M3",
    "types": "M4",
    "errors": "M5",
    "checkeddiv": "M5",
    "checkeddivpromote": "M5",
    "checkeddivwrap": "M5",
    "packages": "M6",
    "refinements": "M6",
    "concurrency": "M7",
    "regions": "R1",
    "closures": "R4",
    "writers": "R4",
    "pools": "R4",
    "asserts": "M8",
    "sharing": "R6",
    "sync": "R6",
    "fmt": "L1",
    "tuples": "L1",
    "funcvalues": "L1",
    "ffi": "L2",
    "consts": "M2",
    "arms": "M2",
    "smallfixes": "L1",
    "trymethod": "M5",
    "pure": "A4",
    "flags": "L2",
    "wc": "L2",
}


def positive(case):
    g = GROUPS[case]
    want = (CASES / f"{case}.expected").read_text().strip()
    b = budget(case)
    src = f"{case}.alx"

    # 1. debug + edit-to-answer
    r, _ = run([ALX, "run", src])
    check(g, f"{case} debug output", r.stdout.strip() == want and r.returncode == 0, f"got {r.stdout.strip()!r} {r.stderr.strip()[:200]}")
    if "edit_to_answer_ms" in b:
        ts = [run([ALX, "run", src])[1] for _ in range(7)]
        m = statistics.median(ts)
        check(g, f"{case} edit-to-answer", m < b["edit_to_answer_ms"], f"median {m:.0f} ms (budget {b['edit_to_answer_ms']:.0f})")

    # 2. release
    r, _ = run([ALX, "build", "--release", src])
    binary = r.stdout.strip()
    r2, _ = run([binary])
    check(g, f"{case} release output", r2.stdout.strip() == want and r2.returncode == 0, f"got {r2.stdout.strip()!r}")
    if "release_vs_ref" in b:
        ref_bin = WORK / f"ref_{case}"
        if not ref_bin.exists():
            subprocess.run(["rustc", "--edition", "2024", "-O", str(REFS / f"{case}.rs"), "-o", str(ref_bin)], check=True, capture_output=True)
        ours, theirs = best_ms([binary], 15), best_ms([str(ref_bin)], 15)
        ratio = ours / theirs
        check(g, f"{case} release vs Rust", ratio <= b["release_vs_ref"], f"{ours:.2f} ms vs {theirs:.2f} ms = {ratio:.2f}x (budget {b['release_vs_ref']}x)")
    if "release_allocs" in b or "release_allocs_max" in b:
        r3, _ = run([binary], env={"ALX_COUNT_ALLOCS": "1"})
        m = re.search(r"alx-allocs: (\d+)", r3.stderr)
        n = int(m.group(1)) if m else -1
        limit = b.get("release_allocs", b.get("release_allocs_max"))
        check(g, f"{case} release allocations", 0 <= n <= limit, f"{n} (limit {limit:.0f})")
    if "release_checked_index" in b:
        rel_c, dbg_c = WORK / f"{case}-rel.c", WORK / f"{case}-dbg.c"
        run([ALX, "build", "--release", "--emit-c", str(rel_c), src])
        run([ALX, "build", "--emit-c", str(dbg_c), src])
        count = lambda p: len(re.findall(r"ALX_IDX(_SET)?\(", p.read_text()))
        rel, dbg = count(rel_c), count(dbg_c)
        check(g, f"{case} index checks removed in release", rel <= b["release_checked_index"] and dbg > 0, f"debug {dbg}, release {rel}")
    if "pmap_speedup" in b:
        cores = os.cpu_count() or 1
        one = best_ms([binary], 5, env={"ALX_THREADS": "1"})
        many = best_ms([binary], 5)
        sp = one / many
        ok = sp >= b["pmap_speedup"] if cores >= 4 else True
        check(g, f"{case} pmap speedup", ok, f"{one:.1f} ms on 1 thread, {many:.1f} ms on {cores} cores = {sp:.1f}x (need {b['pmap_speedup']}x on >=4 cores)")

    # 3. safety net
    r, _ = run([ALX, "run", "--sanitize", src])
    reports = [l for l in r.stderr.splitlines() if "runtime error" in l or "AddressSanitizer" in l]
    check(g, f"{case} ASan/UBSan clean", r.stdout.strip() == want and r.returncode == 0 and not reports, (reports[:1] or [r.stderr.strip()[:200]])[0] if reports or r.returncode else "")
    rs = WORK / f"{case}.rs"
    run([ALX, "build", "--emit-rust", str(rs), src])
    rbin = WORK / f"{case}_oracle"
    rc = subprocess.run(["rustc", "--edition", "2024", "-O", str(rs), "-o", str(rbin)], capture_output=True, text=True)
    if rc.returncode != 0:
        first = next((l for l in rc.stderr.splitlines() if l.startswith("error")), rc.stderr[:200])
        check(g, f"{case} Rust oracle accepts", False, first)
    else:
        r4, _ = run([str(rbin)])
        check(g, f"{case} Rust oracle accepts and agrees", r4.stdout.strip() == want, f"oracle printed {r4.stdout.strip()!r}")


def negative_compile(case, group):
    want = (CASES / f"{case}.expected_error").read_text().strip().splitlines()
    r, ms = run([ALX, "run", f"{case}.alx"])
    got = [l.rstrip() for l in r.stderr.splitlines()[:2]]
    ok = got == [w.rstrip() for w in want] and r.returncode == 1
    check(group, f"{case} first error", ok, "" if ok else f"got {got}")
    return ms


def negative_runtime(case, group):
    lines = (CASES / f"{case}.expected_runtime").read_text().strip().splitlines()
    status = lines[0].split(":", 1)[1].strip()
    r, _ = run([ALX, "run", f"{case}.alx"])
    status_ok = (r.returncode == -6) if status == "abort" else (r.returncode == int(status))
    missing = [l for l in lines[1:] if l not in r.stderr]
    check(group, f"{case} runtime failure", status_ok and not missing, f"exit {r.returncode}" + (f", missing {missing}" if missing else ""))


def memory():
    """R1-R3: call-, loop- and churn-heavy programs run in bounded memory (release builds)."""
    d = CASES / "mem"
    for prog, want, ms in [("calls.alx", "12588314", "R1"), ("loops.alx", "1000000", "R1"), ("fill.alx", "140000", "R2"), ("churn.alx", "100", "R3"), ("pchurn.alx", "50", "R3")]:
        r, _ = run([ALX, "build", "--release", prog], cwd=d)
        binary = r.stdout.strip()
        r2, _ = run([binary], cwd=d, env={"ALX_MEMSTATS": "1"})
        peak = next((int(w.split("=")[1]) for w in r2.stderr.split() if w.startswith("peak=")), None)
        ok = r2.returncode == 0 and r2.stdout.splitlines()[:1] == [want] and peak is not None and peak <= 8 << 20
        check(ms, f"bounded memory: {prog}", ok, f"peak {peak} bytes, stdout {r2.stdout.strip()[:40]!r}")


def std_tests():
    """L1+: every std package's own tests pass, on the JIT and in release builds."""
    std = ROOT / "std"
    pkgs = sorted({f.parent for f in std.rglob("*_test.alx")})
    for d in pkgs:
        name = str(d.relative_to(std))
        for mode in ([], ["--release"]):
            r, _ = run([ALX, "test", *mode, str(d)], cwd=ROOT)
            label = "release" if mode else "jit"
            check("L1", f"std/{name} tests ({label})", r.returncode == 0, r.stdout[-300:] + r.stderr[-300:])


def explain():
    """R7: `alx explain mem` names each allocation's region and why."""
    r, _ = run([ALX, "explain", "mem", "explain.alx"], cwd=CASES / "mem")
    wants = [
        "`cache`'s own region: compacted at the loop at 5:7",
        "6:10    \"line #{i}\"",
        "piles up until then",
        "program region, never freed: it reaches `spawn { \"task #{i}\" }` at 11:7",
        "1 allocation site(s) in the program region; 3 site(s) inside loops that pile up",
    ]
    missing = [w for w in wants if w not in r.stdout]
    check("R7", "explain mem: regions and reasons", r.returncode == 0 and not missing, f"missing {missing}")


def tasks_memory():
    """R5: 100000 tasks (coroutines) in a daisy chain run in bounded memory (release build)."""
    d = CASES / "mem"
    r, _ = run([ALX, "build", "--release", "tasks.alx"], cwd=d)
    binary = r.stdout.strip()
    with open(WORK / "tasks.out", "w+") as fo, open(WORK / "tasks.err", "w+") as fe:
        t0 = time.perf_counter()
        pr = subprocess.Popen([binary], cwd=d, env={**os.environ, "ALX_MEMSTATS": "1"}, stdout=fo, stderr=fe)
        _, status, ru = os.wait4(pr.pid, 0)  # this child's own rusage
        ms = (time.perf_counter() - t0) * 1e3
        fo.seek(0); fe.seek(0)
        out, err = fo.read(), fe.read()
    rss = ru.ru_maxrss if sys.platform == "darwin" else ru.ru_maxrss * 1024  # bytes
    peak = next((int(w.split("=")[1]) for w in err.split() if w.startswith("peak=")), None)
    ok = status == 0 and out.strip() == "100000" and peak is not None and peak <= 64 << 20 and rss < 512 << 20
    check("R5", "100000-task daisy chain", ok, f"peak {peak} bytes, rss {rss >> 20} MiB, {ms:.0f} ms, stdout {out.strip()[:40]!r}")
    r3, _ = run([ALX, "run", "tasks.alx"], cwd=d)
    check("R5", "100000-task daisy chain (JIT)", r3.returncode == 0 and r3.stdout.strip() == "100000", f"exit {r3.returncode}")


def warnings():
    """Unused locals and imports warn; --strict makes them errors."""
    d = CASES / "warn"
    r, _ = run([ALX, "run", "warn.alx"], cwd=d)
    ok = r.returncode == 0 and r.stdout.strip() == "5" and "warning: `pk` is imported but not used" in r.stderr and "warning: `unused` is assigned but never used" in r.stderr and "_quiet" not in r.stderr
    check("M8", "unused warnings", ok, f"exit {r.returncode}, stderr {r.stderr.strip()[:200]!r}")
    r, _ = run([ALX, "run", "--strict", "warn.alx"], cwd=d)
    check("M8", "--strict makes warnings errors", r.returncode == 1 and r.stdout == "" and "error: `pk` is imported but not used" in r.stderr, f"exit {r.returncode}")


def modules():
    d = CASES / "modtest"
    env = {"ALX_MODCACHE": str(d / "cache")}
    want = (d / "main.expected").read_text().strip()
    r, _ = run([ALX, "run", "main.alx"], cwd=d, env=env)
    check("M6", "modtest require+replace+MVS output", r.stdout.strip() == want and r.returncode == 0, f"got {r.stdout.strip()!r} {r.stderr.strip()[:200]}")
    r, _ = run([ALX, "run", "bad.alx"], cwd=d, env=env)
    want = (d / "bad.expected_error").read_text().strip().splitlines()
    got = [l.rstrip() for l in r.stderr.splitlines()[:2]]
    check("M6", "modtest unrequired import error", got == want and r.returncode == 1, f"got {got}")
    r, _ = run([ALX, "run", "main.alx"], cwd=d, env={"ALX_MODCACHE": str(WORK / "empty-cache")})
    check("M6", "modtest missing cache dir error", "isn't in the module cache" in r.stderr and r.returncode == 1, r.stderr.strip()[:200])


def fmt_check():
    paths = sorted(str(p) for p in CASES.glob("*.alx"))
    paths += [str(CASES / "pkgs"), str(CASES / "lib")]
    r, _ = run([ALX, "fmt", "--check", *paths])
    check("M8", "alx fmt --check: cases are canonically formatted", r.returncode == 0 and r.stdout == "", f"exit {r.returncode}: {r.stdout.strip()[:200]}")


def norm_test_output(text):
    """Timings vary: (0.12s) and the final `ok  dir 0.123s` become constants."""
    text = re.sub(r"\(\d+\.\d\ds\)", "(0.00s)", text)
    return re.sub(r" \d+\.\d{3}s$", " 0.000s", text, flags=re.M)


def testing():
    """`alx test`: discovery, assertions, examples, benchmarks, exit codes."""
    d = CASES / "testing"
    r, _ = run([ALX, "test"], cwd=d)
    want = (d / "test.expected").read_text()
    check("M8", "alx test passes (package + tests, imports, examples)", r.returncode == 0 and norm_test_output(r.stdout) == want, f"exit {r.returncode}: {norm_test_output(r.stdout)[-300:]!r} {r.stderr.strip()[:200]}")
    r, _ = run([ALX, "test", "calc_test.alx"], cwd=d)
    check("M8", "alx test FILE", r.returncode == 0 and norm_test_output(r.stdout) == want, f"exit {r.returncode}")
    r, _ = run([ALX, "test", "testing"])
    check("M8", "alx test DIR from elsewhere", r.returncode == 0 and "ok     testing " in r.stdout, f"exit {r.returncode}")
    r, _ = run([ALX, "test", "-run", "clamp"], cwd=d)
    out = norm_test_output(r.stdout)
    check("M8", "alx test -run filters by substring", r.returncode == 0 and "=== RUN   clamp" in out and "RUN   add" not in out and "RUN   printing" not in out, out[:200])
    r, _ = run([ALX, "test", "-bench", "."], cwd=d, env={"ALX_BENCHTIME": "20ms"})
    check("M8", "alx test -bench runs benchmarks", r.returncode == 0 and re.search(r"^Benchmarkadd +\d+ +[\d.]+ ns/op$", r.stdout, re.M) is not None, r.stdout[-200:])
    r, _ = run([ALX, "test", "-benchtime", "10ms", "-bench", "add", "-run", "none"], cwd=d)
    check("M8", "alx test -benchtime, benchmarks only", r.returncode == 0 and re.search(r"^Benchmarkadd +\d+ +[\d.]+ ns/op$", r.stdout, re.M) is not None and "=== RUN" not in r.stdout, r.stdout[-200:])
    r, _ = run([ALX, "test"], cwd=d)
    check("M8", "benchmarks don't run without -bench", "Benchmark" not in r.stdout, "")
    r, _ = run([ALX, "test", "--release"], cwd=d)
    check("M8", "alx test --release (C backend)", r.returncode == 0 and norm_test_output(r.stdout) == want, f"exit {r.returncode}: {r.stderr.strip()[:200]}")

    d = CASES / "testing_fail"
    r, _ = run([ALX, "test"], cwd=d)
    want = (d / "test.expected").read_text()
    check("M8", "alx test failures: output and exit code 1", r.returncode == 1 and norm_test_output(r.stdout) == want, f"exit {r.returncode}: {norm_test_output(r.stdout)[-300:]!r}")
    r, _ = run([ALX, "test", "--release"], cwd=d)
    check("M8", "alx test --release failures: exit code 1", r.returncode == 1 and norm_test_output(r.stdout) == want, f"exit {r.returncode}")

    # `test` blocks belong in test files.
    tmp = WORK / "misplaced"
    tmp.mkdir(exist_ok=True)
    (tmp / "a.alx").write_text('test "x" {\n  assert true\n}\n')
    r, _ = run([ALX, "run", "a.alx"], cwd=tmp)
    check("M8", "test block outside a _test.alx file is an error", r.returncode == 1 and "`test` blocks belong in a `*_test.alx` file" in r.stderr, r.stderr.strip()[:200])


def promote_overhead():
    tmp = WORK / "promote"
    tmp.mkdir(exist_ok=True)
    src = (CASES / "pe001.alx").read_text()
    (tmp / "plain.alx").write_text(src)
    (tmp / "prom.alx").write_text("#![overflow(promote)]\n" + src)
    b1 = run([ALX, "build", "--release", "plain.alx"], cwd=tmp)[0].stdout.strip()
    b2 = run([ALX, "build", "--release", "prom.alx"], cwd=tmp)[0].stdout.strip()
    out = run([b2], cwd=tmp)[0].stdout.strip()
    check("A3", "promote-mode pe001 output", out == "233168", out)
    t1, t2 = best_ms([b1], 31, cwd=tmp), best_ms([b2], 31, cwd=tmp)
    check("A3", "promote overhead on word-sized values", t2 <= 1.2 * t1, f"{t2:.2f} ms vs {t1:.2f} ms = {t2 / t1:.2f}x (limit 1.2x)")


def a6():
    # Edit-to-answer: alternate an edit, rerun.
    tmp = WORK / "edit"
    tmp.mkdir(exist_ok=True)
    src = (CASES / "pe001.alx").read_text()
    times, outs = [], set()
    for i in range(10):
        (tmp / "pe001.alx").write_text(src.replace("1000", "999" if i % 2 else "1000"))
        r, ms = run([ALX, "run", "pe001.alx"], cwd=tmp)
        times.append(ms)
        outs.add(r.stdout.strip())
    m = statistics.median(times)
    check("A6", "edit-to-answer", m < 300 and outs == {"233168", "232169"}, f"median {m:.0f} ms over 10 edits; outputs {sorted(outs)}")

    # Error round-trip.
    ms = statistics.median([negative_compile("pe001.typo", "A6") for _ in range(3)])
    check("A6", "error round-trip time", ms < 200, f"{ms:.0f} ms")

    # Library reuse: fresh cache, then a second run must not rebuild the library.
    lib = WORK / "libreuse"
    if lib.exists():
        shutil.rmtree(lib)
    shutil.copytree(CASES, lib, ignore=shutil.ignore_patterns(".alx-cache"))
    # (`alx run` JITs the whole program in memory; native builds compile
    # libraries separately and cache them.)
    r1, _ = run([ALX, "build", "-v", "pe007.alx"], cwd=lib)
    r2, _ = run([ALX, "build", "-v", "pe007.alx"], cwd=lib)
    out = run([r2.stdout.strip()], cwd=lib)[0].stdout.strip() if r2.returncode == 0 else ""
    ok = "lib/primes: compiling" in r1.stderr and "lib/primes: cached" in r2.stderr and out == "104743"
    check("A6", "library reuse", ok, " / ".join(l for l in r2.stderr.splitlines() if "lib/" in l))

    # No setup: a fresh directory containing only pe001.alx.
    fresh = WORK / "fresh"
    if fresh.exists():
        shutil.rmtree(fresh)
    fresh.mkdir()
    shutil.copy(CASES / "pe001.alx", fresh / "pe001.alx")
    r, _ = run([ALX, "run", "pe001.alx"], cwd=fresh)
    entries = sorted(p.name for p in fresh.iterdir())
    check("A6", "no setup", r.stdout.strip() == "233168" and entries in ([".alx-cache", "pe001.alx"], ["pe001.alx"]), f"directory now holds {entries}")

    # Answer check.
    good = run([ALX, "run", "pe010.alx", "--expect", "142913828922"])[0].returncode
    bad = run([ALX, "run", "pe010.alx", "--expect", "1"])[0].returncode
    check("A6", "--expect", good == 0 and bad == 1, f"exit {good} on match, {bad} on mismatch")


def licenses():
    rt = ROOT / "compiler" / "runtime"
    allowed = {"Apache-2.0 WITH LLVM-exception", "Unlicense", "0BSD", "MIT-0", "public domain"}
    manifest = {}
    for line in (rt / "LICENSES").read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            path, lic = line.split(None, 1)
            manifest[path] = lic.strip()
    files = sorted(str(p.relative_to(rt)) for p in rt.rglob("*") if p.is_file() and p.name != "LICENSES")
    unlisted = [f for f in files if f not in manifest]
    bad = {f: l for f, l in manifest.items() if l not in allowed}
    check("all", "runtime license manifest", not unlisted and not bad, f"unlisted {unlisted}, disallowed {bad}" if unlisted or bad else f"{len(files)} files, all allowed")


def main():
    filt = sys.argv[sys.argv.index("-k") + 1] if "-k" in sys.argv else ""
    print("building the compiler (release)...", flush=True)
    subprocess.run(["cargo", "build", "--release", "-q"], cwd=ROOT, check=True)
    shutil.rmtree(CASES / ".alx-cache", ignore_errors=True)
    for case in GROUPS:
        if filt in case:
            print(f"== {case}", flush=True)
            positive(case)
    if not filt or "neg" in filt:
        print("== negative cases", flush=True)
        negative_compile("pe014.bad1", "A4")
        negative_compile("sharing.bad1", "R6")
        negative_compile("sync.bad1", "R6")
        negative_compile("sync.bad2", "R6")
        negative_compile("sync.bad3", "R6")
        negative_compile("sync.bad4", "R6")
        negative_compile("sharing.bad2", "R6")
        negative_compile("sharing.bad3", "R6")
        negative_compile("sharing.bad4", "R6")
        negative_compile("pe014.bad2", "A4")
        negative_compile("pe004.bad", "A4")
        for i in range(1, 9):
            negative_compile(f"pure.bad{i}", "A4")
        negative_runtime("pe020.bad", "A3")
        negative_runtime("pe022.missing", "A5")
        negative_compile("ffi.bad1", "L2")
        negative_compile("ffi.bad2", "L2")
        negative_compile("ffi.bad3", "L2")
        negative_compile("ints.bad1", "M1")
        negative_compile("ints.bad2", "M1")
        negative_runtime("ints.overflow", "M1")
        negative_compile("collections.bad1", "M3")
        negative_compile("collections.bad2", "M3")
        negative_runtime("collections.oob", "M3")
        negative_compile("types.bad1", "M4")
        negative_compile("types.bad2", "M4")
        negative_compile("types.bad3", "M4")
        negative_compile("errors.bad1", "M5")
        negative_compile("errors.bad2", "M5")
        negative_compile("errors.bad3", "M5")
        negative_runtime("errors.die", "M5")
        negative_compile("packages.bad1", "M6")
        negative_compile("packages.bad2", "M6")
        negative_compile("packages.bad3", "M6")
        negative_compile("refinements.bad1", "M6")
        negative_runtime("concurrency.deadlock", "M7")
        negative_compile("consts.bad1", "M2")
        negative_compile("consts.bad2", "M2")
        negative_compile("arms.bad1", "M2")
        negative_compile("arms.bad2", "M2")
        negative_compile("smallfixes.bad1", "L1")
    if not filt or "mod" in filt:
        print("== modules", flush=True)
        modules()
        warnings()
        memory()
        explain()
        std_tests()
        tasks_memory()
        fmt_check()
        print("== alx test", flush=True)
        testing()
    if not filt or "A3" in filt:
        print("== A3 promote overhead", flush=True)
        promote_overhead()
    if not filt or "A6" in filt:
        print("== A6 day-to-day loop", flush=True)
        a6()
    if not filt:
        licenses()
    print()
    groups = sorted({r[0] for r in results})
    print("| group | passed | failed |")
    print("|---|---|---|")
    for g in groups:
        rs = [r for r in results if r[0] == g]
        print(f"| {g} | {sum(r[2] for r in rs)} | {sum(not r[2] for r in rs)} |")
    failed = [r for r in results if not r[2]]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    for g, n, _, d in failed:
        print(f"  FAIL {g} {n}: {d}")
    shutil.rmtree(WORK, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

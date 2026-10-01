#!/usr/bin/env python3
"""Probe 07: map rustc JSON diagnostics on emitted Rust back to .alx source.

Source map = trailing `// @LINE:COL key=val ...` markers the emitter writes on
generated lines. Positions alone come from the marker; anything semantic
(which pool a handle came from, which Alexandrite method a Rust call came
from) comes from the tags. Each rewrite records which it needed.

usage: map.py            run every case, print diagnostics + summary table
"""

import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).parent
CASES = HERE / "cases"
TARGET = HERE.parent / "target" / "debug"
OUT = HERE.parent / "target" / "probe07"

MARK = re.compile(r"//\s*@(\d+):(\d+)\s*(.*)$")

# Rust type -> Alexandrite type. Applied repeatedly so nesting works.
TYPES = [
    (r"Vec<([^<>]+)>", r"[\1]"),
    (r"Option<([^<>]+)>", r"\1?"),
    (r"Result<([^<>,]+), ([^<>]+)>", r"\1!\2"),
    (r"Handle<'[^,]+, ([^<>]+)>", r"@\1"),
    (r"BPool<'[^,]+, ([^<>]+)>", r"pool(\1)"),
    (r"&str\b", "Str"),
    (r"\bString\b", "Str"),
    (r"\bi64\b", "Int"),
    (r"\bi32\b", "I32"),
]


def alx_type(t):
    prev = None
    while prev != t:
        prev = t
        for pat, rep in TYPES:
            t = re.sub(pat, rep, t)
    return t


def markers(rs):
    out = {}
    for n, line in enumerate(rs.read_text().splitlines(), 1):
        m = MARK.search(line)
        if m:
            tags = dict(kv.split("=", 1) for kv in m.group(3).split() if "=" in kv)
            out[n] = (int(m.group(1)), int(m.group(2)), tags)
    return out


def locate(marks, line):
    """Map a Rust line to (alx_line, alx_col, tags, exact?)."""
    if line in marks:
        return (*marks[line], True)
    prior = [n for n in marks if n < line]
    if prior:
        return (*marks[max(prior)], False)
    return None


def rustc(rs):
    OUT.mkdir(parents=True, exist_ok=True)
    cmd = [
        "rustc", "--edition", "2024", "--crate-type", "lib", "--emit=metadata",
        "-o", str(OUT / (rs.stem + ".rmeta")), "--error-format=json",
        "--extern", f"alx_rt={TARGET / 'libalx_rt.rlib'}", "-L", str(TARGET / "deps"),
        str(rs),
    ]
    r = subprocess.run(cmd, capture_output=True, text=True)
    diags = [json.loads(l) for l in r.stderr.splitlines() if l.startswith("{")]
    return [d for d in diags if d["level"] == "error" and d.get("spans")]


def code(d):
    return (d.get("code") or {}).get("code")


def name_in(msg):
    m = re.search(r"`([^`]+)`", msg)
    return m.group(1) if m else None


# ---------- rewrite rules: (diagnostics, marks) -> list of alx errors ----------
# Each alx error: dict(at=(line,col), msg, notes, needs={"position","types","tags"})


def find_tag(marks, key, val=None):
    for n, (l, c, t) in marks.items():
        if key in t and (val is None or t[key] == val):
            return n, (l, c, t)
    return None


def rule_moved(d, marks):
    var = name_in(d["message"])
    spans = {s["label"]: s for s in d["spans"]}
    moved = next(s for lbl, s in spans.items() if lbl and "moved here" in lbl)
    used = next(s for s in d["spans"] if s["is_primary"])
    ml = locate(marks, moved["line_start"])
    ul = locate(marks, used["line_start"])
    callee = ml[2].get("call", "?")
    return [dict(
        at=ul[:2],
        msg=f"`{var}` was handed off to `{callee}` and can't be used after",
        notes=[
            f"{ml[0]}:{ml[1]}: `{callee}` takes `sink {var}`, so `{var}` is gone after this call",
            f"to keep `{var}`, drop `sink` from `{callee}` (borrow instead) or pass a copy",
        ],
        needs={"position", "tags"},
        exact=ul[3] and ml[3],
    )]


def rule_brand(diags, marks):
    """E0521s and lifetime errors from branded pools. rustc's spans don't
    name the misuse; the emitter's tags do."""
    out = []
    for n, (l, c, t) in marks.items():
        if "index" in t and "handle" in t:
            src = find_tag(marks, "handle", t["handle"])
            from_pool = src[1][2].get("pool") if src else "?"
            if from_pool != t["index"]:
                out.append(dict(
                    at=(l, c),
                    msg=f"`{t['handle']}` is a handle into `{from_pool}`, used on `{t['index']}`",
                    notes=[f"{src[1][0]}:{src[1][1]}: `{t['handle']}` was made by `{from_pool} <<`"],
                    needs={"tags"},
                    exact=True,
                ))
        if t.get("escapes") == "return":
            pool = t.get("pool", "?")
            opened = find_tag(marks, "pool", pool)
            out.append(dict(
                at=(l, c),
                msg=f"a handle into `{pool}` can't outlive `{pool}`",
                notes=[
                    f"{opened[1][0]}:{opened[1][1]}: `{pool}` ends with this method",
                    "return a value (it is copied out) or take the pool as a parameter",
                ],
                needs={"tags"},
                exact=True,
            ))
    return out


def rule_mismatch(d, marks):
    prim = next(s for s in d["spans"] if s["is_primary"])
    m = re.search(r"expected `(.+)`, found `(.+)`", prim["label"] or "")
    want, got = (alx_type(m.group(1)), alx_type(m.group(2))) if m else ("?", "?")
    blk = find_tag(marks, "block_result")
    fn = find_tag(marks, "def")
    if blk:
        _, (l, c, t) = blk
        elem = re.sub(r"^\[(.*)\]$", r"\1", got)
        return [dict(
            at=(l, c),
            msg=f"the block passed to `{t['block_result']}` returns {elem}, but `{fn[1][2]['def']}` returns {want}",
            notes=[f"{fn[1][0]}:{fn[1][1]}: return type declared here"],
            needs={"position", "types", "tags"},
            exact=True,
        )]
    loc = locate(marks, prim["line_start"])
    return [dict(at=loc[:2], msg=f"expected {want}, found {got}", notes=[],
                 needs={"position", "types"}, exact=loc[3])]


def rule_pure(diags, marks):
    """Every const-fn violation on a line collapses into one error about the
    Alexandrite call on that line."""
    by_line = {}
    for d in diags:
        for s in d["spans"]:
            if s["is_primary"]:
                by_line.setdefault(s["line_start"], []).append(d)
    out = []
    fn = find_tag(marks, "def")
    for line, ds in by_line.items():
        loc = locate(marks, line)
        t = loc[2]
        call, rust = t.get("call"), t.get("rust")
        out.append(dict(
            at=loc[:2],
            msg=f"`#[pure] def {fn[1][2]['def']}` calls `{call}`, which isn't pure",
            notes=[
                f"`{call}` is Rust's `{rust}`, which allocates",
                f"mark the call `trust_pure` if you vouch for it",
                f"({len(ds)} rustc errors folded into this one)",
            ],
            needs={"position", "tags"},
            exact=loc[3],
        ))
    return out


def rewrite(diags, marks):
    errs = []
    brand = [d for d in diags if code(d) == "E0521" or "may not live long enough" in d["message"]]
    pure = [d for d in diags if code(d) in ("E0015", "E0493")]
    for d in diags:
        if code(d) == "E0382":
            errs += rule_moved(d, marks)
        elif code(d) == "E0308":
            errs += rule_mismatch(d, marks)
    if brand:
        errs += rule_brand(brand, marks)
    if pure:
        errs += rule_pure(pure, marks)
    return errs


def show(alx, e):
    src = alx.read_text().splitlines()
    l, c = e["at"]
    print(f"error: {e['msg']}")
    print(f"  --> {alx.name}:{l}:{c}{'' if e['exact'] else '  (approximate)'}")
    print(f"   | {src[l - 1]}")
    print(f"   | {' ' * (c - 1)}^")
    for n in e["notes"]:
        print(f"   = {n}")
    print()


def main():
    subprocess.run(["cargo", "build", "-q", "-p", "alx_rt"], cwd=HERE.parent, check=True)
    rows = []
    for rs in sorted(CASES.glob("*.rs")):
        alx = rs.with_suffix(".alx")
        marks = markers(rs)
        diags = rustc(rs)
        prim = [s for d in diags for s in d["spans"] if s["is_primary"]]
        labels = [s for d in diags for s in d["spans"] if not s["is_primary"] and s["label"]]
        exact = sum(1 for s in prim if s["line_start"] in marks)
        lab_ok = sum(1 for s in labels if s["line_start"] in marks)
        errs = rewrite(diags, marks)
        print(f"===== {rs.stem}: {len(diags)} rustc error(s) -> {len(errs)} alx error(s)\n")
        for e in errs:
            show(alx, e)
        needs = sorted(set().union(*(e["needs"] for e in errs))) if errs else []
        rows.append((rs.stem, len(diags), len(errs), f"{exact}/{len(prim)}", f"{lab_ok}/{len(labels)}", "+".join(needs)))
    print("| case | rustc errors | alx errors | primary spans on a marker | labels on a marker | info needed |")
    print("|---|---|---|---|---|---|")
    for r in rows:
        print("| " + " | ".join(str(x) for x in r) + " |")


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Build the Alexandrite notebook site: docs/site/index.html.

Embeds ROADMAP.md, GO-VS-RUBY.md and docs/milestones/M*.md into one page
(rendered client-side with marked). Run after writing a milestone doc, then
republish docs/site/index.html.
"""
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent / "index.html"


def title_of(md, fallback):
    m = re.search(r"^# (.+)$", md, re.M)
    return m.group(1).strip() if m else fallback


roadmap = (ROOT / "ROADMAP.md").read_text()
done = {p.stem for p in (ROOT / "docs" / "milestones").glob("M*.md")}
milestones = []
for row in re.findall(r"^\| (M\d+) \| \*\*(.+?)\*\* \| (.+?) \|$", roadmap, re.M):
    mid, name, contents = row
    milestones.append({"id": mid.lower(), "num": mid, "name": name, "contents": contents, "done": mid in done})

docs = [
    {"id": "roadmap", "title": "Roadmap", "group": "plan", "md": roadmap},
    {"id": "decisions", "title": "Go vs Ruby decisions", "group": "plan", "md": (ROOT / "GO-VS-RUBY.md").read_text()},
]
for p in sorted((ROOT / "docs" / "milestones").glob("M*.md"), key=lambda p: int(p.stem[1:])):
    md = p.read_text()
    docs.append({"id": p.stem.lower(), "title": title_of(md, p.stem), "group": "log", "md": md})

payload = json.dumps({"docs": docs, "milestones": milestones}).replace("</", "<\\/")
html = (Path(__file__).resolve().parent / "template.html").read_text().replace("/*DATA*/null", payload)
OUT.write_text(html)
print(f"wrote {OUT} ({len(docs)} docs, {sum(m['done'] for m in milestones)}/{len(milestones)} milestones done)")

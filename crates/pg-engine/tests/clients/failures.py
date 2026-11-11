"""failures.py <junit.xml or a directory of them> [<pg results .txt>]: the failing tests' first message line, grouped
by message (most common first). With PostgreSQL's results, tests failing there too are left out."""
import collections
import pathlib
import re
import sys
import xml.etree.ElementTree as ET

skip = set()
if len(sys.argv) > 2:
    for line in open(sys.argv[2]):
        st, _, name = line.rstrip("\n").partition(" ")
        if st == "FAIL":
            skip.add(name)
groups = collections.defaultdict(list)
src = pathlib.Path(sys.argv[1])
files = sorted(src.rglob("*.xml")) if src.is_dir() else [src]
cases = [tc for f in files for tc in ET.parse(f).getroot().iter("testcase")]
for tc in cases:
    name = (tc.get("classname") or "") + "::" + (tc.get("name") or "")
    if name in skip:
        continue
    for c in tc:
        if c.tag in ("failure", "error"):
            msg = (c.get("message") or c.text or "").strip().splitlines()
            m = msg[0] if msg else "?"
            m = re.sub(r"0x[0-9a-f]+", "0x?", m)[:220]
            groups[m].append(name)
            break
for m, names in sorted(groups.items(), key=lambda kv: -len(kv[1])):
    print(f"{len(names):4}  {m}")
    for n in names[:3]:
        print(f"        {n}")

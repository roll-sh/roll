"""gofail.py <x.gojson> [<pg results .txt>]: each failing Go test's output (trimmed), leaving
out tests that fail on PostgreSQL too."""
import collections
import json
import sys

skip = set()
if len(sys.argv) > 2:
    for line in open(sys.argv[2]):
        st, _, name = line.rstrip("\n").partition(" ")
        if st == "FAIL":
            skip.add(name)
out = collections.defaultdict(list)
failed = []
for line in open(sys.argv[1], errors="replace"):
    try:
        ev = json.loads(line)
    except ValueError:
        continue
    t = ev.get("Test")
    if not t:
        continue
    key = ev["Package"] + "::" + t
    if ev.get("Action") == "output":
        out[key].append(ev["Output"].rstrip("\n"))
    elif ev.get("Action") == "fail" and key not in skip and "/" not in t:
        failed.append(key)
for k in failed:
    lines = [l for l in out[k] if l.strip() and not l.lstrip().startswith(("=== RUN", "--- FAIL", "=== PAUSE", "=== CONT"))]
    print(k)
    for l in lines[:12]:
        print("    " + l.strip()[:240])

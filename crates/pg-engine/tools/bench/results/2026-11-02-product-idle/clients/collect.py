"""collect.py <dir>: JUnit XML files (and PASS/FAIL/SKIP .lst files) in <dir> as sorted
`PASS|FAIL|SKIP <test id>` lines."""
import pathlib
import sys
import xml.etree.ElementTree as ET

out = {}
for p in sorted(pathlib.Path(sys.argv[1]).rglob("*")):
    if p.suffix == ".lst":
        for line in p.read_text(errors="replace").splitlines():
            st, _, name = line.partition(" ")
            if st in ("PASS", "FAIL", "SKIP") and name:
                out[name] = st
    elif p.suffix == ".gojson":
        # `go test -json`: one event per line; the last pass / fail / skip per test counts.
        import json

        for line in p.read_text(errors="replace").splitlines():
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            if ev.get("Test") and ev.get("Action") in ("pass", "fail", "skip"):
                st = {"pass": "PASS", "fail": "FAIL", "skip": "SKIP"}[ev["Action"]]
                out[ev["Package"] + "::" + ev["Test"]] = st
    elif p.suffix in (".xml", ".trx"):
        try:
            root = ET.parse(p).getroot()
        except ET.ParseError:
            continue
        # .NET TRX: UnitTestResult testName / outcome.
        for r in root.iter():
            if r.tag.endswith("UnitTestResult"):
                o = r.get("outcome", "")
                st = {"Passed": "PASS", "Failed": "FAIL"}.get(o, "SKIP")
                out[r.get("testName", "?")] = st
        for tc in root.iter("testcase"):
            name = (tc.get("classname") or "") + "::" + (tc.get("name") or "")
            tags = {c.tag for c in tc}
            if tags & {"failure", "error"}:
                st = "FAIL"
            elif "skipped" in tags:
                st = "SKIP"
            else:
                st = "PASS"
            # A test reported twice (reruns): a failure wins.
            if out.get(name) != "FAIL":
                out[name] = st
for name in sorted(out):
    print(out[name], name)

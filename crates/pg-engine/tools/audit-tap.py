#!/usr/bin/env python3
"""Audit numbered TAP output; Rust wrapper success is not a TAP result."""
import argparse
import json
import re
from pathlib import Path

PG_DUMP_PLANS = {
    "001_basic.pl": 96,
    "002_pg_dump.pl": 14609,
    "004_pg_dump_parallel.pl": 4,
    "005_pg_dump_filterfile.pl": 108,
}


def tap(path, expected):
    text = path.read_text()
    plans = []
    results = []
    issues = []
    for lineno, line in enumerate(text.splitlines(), 1):
        if re.match(r"\s*Bail out!", line, re.I):
            issues.append(f"line {lineno}: bailout")
        if m := re.fullmatch(r"1\.\.(\d+)(?:\s+#.*)?", line):
            plans.append((lineno, int(m[1])))
            if re.search(r"\s+#\s*(SKIP|TODO)\b", line, re.I):
                issues.append(f"line {lineno}: skipped/TODO plan")
        if m := re.match(r"^(not ok|ok)\s+(\d+)(?:\s|$)", line):
            results.append((lineno, int(m[2]), m[1] == "ok"))
            if re.search(r"\s+#\s*(SKIP|TODO)\b", line, re.I):
                issues.append(f"line {lineno}: skipped/TODO work")
        elif re.match(r"^(not ok|ok)(?:\s|$)", line):
            issues.append(f"line {lineno}: missing/invalid result number")
    if len(plans) != 1 or plans[0][1] != expected:
        issues.append(f"expected one 1..{expected} plan; got {plans}")
    if [r[1] for r in results] != list(range(1, expected + 1)):
        issues.append(f"expected exactly results 1..{expected}; got {len(results)} results")
    if len(plans) == 1 and results and results[0][0] < plans[0][0] < results[-1][0]:
        issues.append("plan occurs in the middle of results")
    failed = sum(not r[2] for r in results)
    if failed:
        issues.append(f"{failed} failed TAP results")
    return {"path": str(path), "expected": expected, "passed": len(results) - failed,
            "failed": failed, "plans": [p[1] for p in plans], "issues": issues}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    one = sub.add_parser("tap")
    one.add_argument("path", type=Path)
    one.add_argument("--expected", type=int, required=True)
    dump = sub.add_parser("pg-dump")
    dump.add_argument("log", type=Path)
    dump.add_argument("raw_dir", type=Path)
    args = parser.parse_args()
    suites = {}
    issues = []
    if args.mode == "tap":
        if args.expected < 1:
            issues.append("expected plan must be positive")
        else:
            try:
                suites[args.path.name] = tap(args.path, args.expected)
            except (OSError, UnicodeError) as error:
                issues.append(str(error))
    else:
        try:
            text = args.log.read_text()
            summaries = re.findall(r"^(\S+\.pl): (\d+) passed, (\d+) failed$", text, re.M)
            if len(summaries) != len(PG_DUMP_PLANS) or {s[0] for s in summaries} != set(PG_DUMP_PLANS):
                issues.append("missing, duplicate or unexpected TAP suite summaries")
            for name, passed, failed in summaries:
                if name not in PG_DUMP_PLANS or (int(passed), int(failed)) != (PG_DUMP_PLANS[name], 0):
                    issues.append(f"{name}: summary reports {passed} passed, {failed} failed")
        except (OSError, UnicodeError) as error:
            issues.append(str(error))
        for name, expected in PG_DUMP_PLANS.items():
            try:
                suites[name] = tap(args.raw_dir / f"tap-{name}.out", expected)
            except (OSError, UnicodeError) as error:
                issues.append(str(error))
    for name, suite in suites.items():
        issues.extend(f"{name}: {issue}" for issue in suite["issues"])
    result = {"status": "FAIL" if issues else "PASS", "issues": issues, "suites": suites}
    print(json.dumps(result, indent=2))
    raise SystemExit(bool(issues))


if __name__ == "__main__":
    main()

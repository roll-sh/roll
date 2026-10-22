"""Compare a ceph/s3-tests junit report with tests/s3-conformance/expected.txt.

usage: compare.py <expected.txt> <results.txt> <junit.xml>...

expected.txt lists the tests known not to pass, one per line: `<file>::<test>  <reason>`.
Blank lines and `#` comments are ignored. Exits 1 on a regression (a test failing that isn't
listed) or an unexpected pass (a listed test passing), so the file always matches reality.
Writes every test's outcome to results.txt.
"""
import sys
import xml.etree.ElementTree as ET


def outcomes(paths):
    out = {}
    testcases = [tc for path in paths for tc in ET.parse(path).getroot().iter("testcase")]
    for tc in testcases:
        f = (tc.get("file") or tc.get("classname", "").replace(".", "/") + ".py").split("/")[-1]
        name = f"{f}::{tc.get('name')}"
        status, msg = "passed", ""
        for child in tc:
            if child.tag in ("failure", "error", "skipped"):
                status = {"failure": "failed"}.get(child.tag, child.tag)
                msg = " ".join((child.get("message") or "").split())[:240]
                break
        # A test can fail in its call and error in teardown: the worst outcome counts.
        if name not in out or out[name][0] == "passed":
            out[name] = (status, msg)
    return out


def expected(path):
    known = {}
    for n, line in enumerate(open(path), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        test, _, reason = line.partition(" ")
        if test in known:
            sys.exit(f"{path}:{n}: {test} is listed twice")
        known[test] = reason.strip()
    return known


def main():
    expected_path, results_path, reports = sys.argv[1], sys.argv[2], sys.argv[3:]
    got = outcomes(reports)
    known = expected(expected_path)
    counts = {}
    with open(results_path, "w") as f:
        for name in sorted(got):
            status, msg = got[name]
            counts[status] = counts.get(status, 0) + 1
            f.write(f"{name} {status} {msg}".rstrip() + "\n")
    ran = sum(counts.values())
    passed = counts.get("passed", 0)
    print(f"s3-tests: {ran} selected, {passed} passed, {counts.get('failed', 0)} failed, "
          f"{counts.get('error', 0)} errors, {counts.get('skipped', 0)} skipped "
          f"({100 * passed / max(ran - counts.get('skipped', 0), 1):.1f}% of those that ran)")
    regressions = sorted(n for n, (s, _) in got.items() if s in ("failed", "error") and n not in known)
    fixed = sorted(n for n, (s, _) in got.items() if s == "passed" and n in known)
    missing = sorted(n for n in known if n not in got)
    for n in regressions:
        print(f"REGRESSION {n}: {got[n][1]}")
    for n in fixed:
        print(f"UNEXPECTED PASS {n}: remove it from {expected_path}")
    for n in missing:
        print(f"NOT RUN {n}: listed in {expected_path} but not selected (renamed or deselected?)")
    if regressions or fixed or missing:
        print(f"s3-tests: results differ from {expected_path} (every outcome is in {results_path})")
        sys.exit(1)
    print(f"s3-tests: matches {expected_path}")


main()

"""Compare a ceph/s3-tests junit report with tests/s3-conformance/expected.txt.

usage: compare.py [--backend local|localstack] <expected.txt> <results.txt> <junit.xml>...

expected.txt lists the tests known not to pass, one per line: `<file>::<test>  <category>: <reason>`.
A category ending in `@<backend>` (`env@local:`) only applies to that backend; the others apply
to every backend. Blank lines and `#` comments are ignored. Exits 1 on a regression (a test failing that isn't
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


BACKENDS = ("local", "localstack")


def expected(path, backend):
    known = {}
    seen = set()
    for n, line in enumerate(open(path), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        test, _, reason = line.partition(" ")
        reason = reason.strip()
        category = reason.partition(":")[0]
        only = category.partition("@")[2]
        if only and only not in BACKENDS:
            sys.exit(f"{path}:{n}: unknown backend {only!r} (one of {', '.join(BACKENDS)})")
        key = (test, only)
        if key in seen or (test, "") in seen or (not only and any(t == test for t, _ in seen)):
            sys.exit(f"{path}:{n}: {test} is listed twice")
        seen.add(key)
        if not only or only == backend:
            known[test] = reason
    return known


def main():
    args = sys.argv[1:]
    backend = "local"
    if args[:1] == ["--backend"]:
        backend, args = args[1], args[2:]
    if backend not in BACKENDS:
        sys.exit(f"unknown backend {backend!r}")
    expected_path, results_path, reports = args[0], args[1], args[2:]
    got = outcomes(reports)
    known = expected(expected_path, backend)
    counts = {}
    with open(results_path, "w") as f:
        for name in sorted(got):
            status, msg = got[name]
            counts[status] = counts.get(status, 0) + 1
            f.write(f"{name} {status} {msg}".rstrip() + "\n")
    ran = sum(counts.values())
    passed = counts.get("passed", 0)
    print(f"s3-tests ({backend} backend): {ran} selected, {passed} passed, {counts.get('failed', 0)} failed, "
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

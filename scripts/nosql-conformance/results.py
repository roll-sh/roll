"""pytest plugin: write one `test_id RESULT` line per test, sorted, to $ROLL_CONFORMANCE_RESULTS.

RESULT is PASS, FAIL, SKIP, XFAIL, XPASS or ERROR (setup/teardown failure). XPASS is a strict
xfail test that passed (pytest reports it as a failure): the suite marks a ScyllaDB bug that the
target doesn't have. Loaded with `-p results` (PYTHONPATH=/runner in the runner image).
"""

import os

_results = {}


def _record(nodeid, result):
    _results[nodeid] = result


def pytest_runtest_logreport(report):
    xfail = hasattr(report, "wasxfail")
    prev = _results.get(report.nodeid)
    if report.when == "setup":
        if report.failed:
            _record(report.nodeid, "ERROR")
        elif report.skipped:
            _record(report.nodeid, "XFAIL" if xfail else "SKIP")
    elif report.when == "call":
        if report.passed:
            _record(report.nodeid, "PASS")
        elif report.failed:
            strict_xpass = str(report.longrepr).startswith("[XPASS(strict)]")
            _record(report.nodeid, "XPASS" if strict_xpass else "FAIL")
        else:
            _record(report.nodeid, "XFAIL" if xfail else "SKIP")
    elif report.when == "teardown" and report.failed and prev in (None, "PASS"):
        _record(report.nodeid, "ERROR")


def pytest_collectreport(report):
    # A module that fails to import is one ERROR line for the module.
    if report.failed:
        _record(report.nodeid, "ERROR")


def pytest_sessionfinish(session, exitstatus):
    path = os.environ.get("ROLL_CONFORMANCE_RESULTS")
    if not path:
        return
    with open(path, "w") as f:
        for nodeid in sorted(_results):
            f.write(f"{nodeid} {_results[nodeid]}\n")

#!/usr/bin/env python3
"""Exercise suite ordering and failures without services or real benchmarks."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile


STUB = """#!/bin/bash
printf '%s\\n' "$*" >> "$BENCH_DATA/calls"
echo 'stub stdout'
echo 'stub stderr' >&2
if [[ -n "${FAKE_AT:-}" && $(wc -l < "$BENCH_DATA/calls") != "$FAKE_AT" ]]; then FAKE=ok; fi
if [[ "$1" == pgbench ]]; then
  if [[ "$*" == *" -i "* ]]; then echo initialized; exit "${FAKE_INIT_EXIT:-0}"; fi
  printf 'tps = %s\\nlatency average = 10 ms\\nnumber of failed transactions: %s\\n' "${FAKE_TPS:-100}" "${FAKE_FAILED:-0}"
else
  if [[ "$FAKE" != missing ]]; then
    printf 'RESULT\\tx\\tanalytic\\tx\\trows=1'
    if [[ "$FAKE" == error ]]; then printf '\\terror=bad'; fi
    if [[ "$FAKE" == errors ]]; then printf '\\terrors=1'; fi
    printf '\\n'
  fi
fi
[[ "$FAKE" != producer ]]
"""


def check(suite):
    with tempfile.TemporaryDirectory(prefix="bench-suite-") as directory:
        root = Path(directory)
        stub = root / "run.sh"
        stub.write_text(STUB)
        stub.chmod(0o755)
        cases = [
            ("inproc-fixtures", "analytic", "ok", {}),
            ("pgbench-good", "pgbench", "ok", {}),
            ("producer", "analytic", "producer", {}),
            ("missing", "analytic", "missing", {}),
            ("error", "analytic", "error", {}),
            ("errors", "analytic", "errors", {}),
            ("pgbench-failed", "pgbench", "ok", {"FAKE_FAILED": "1"}),
            ("pgbench-init", "pgbench", "ok", {"FAKE_INIT_EXIT": "9"}),
            ("pgbench-zero", "pgbench", "ok", {"FAKE_TPS": "0"}),
        ]
        for target in ("inproc", "pg", "engine"):
            cases.append((f"oltp-{target}", "oltp", "ok", {"TARGET": target}))
        for name, fake, at in [("read", "errors", "2"), ("reload", "producer", "3"),
                               ("write", "error", "4")]:
            cases.append((f"oltp-{name}-failure", "oltp", fake,
                          {"TARGET": "inproc", "FAKE_AT": at}))
        for name, part, fake, extra in cases:
            data = root / name
            data.mkdir()
            env = dict(os.environ, BENCH_DATA=str(data), SUITE=part, FAKE=fake,
                       FAKE_FAILED="0", FAKE_INIT_EXIT="0", FAKE_TPS="100", FAKE_AT="",
                       RUN=str(stub), OLTP_ROWS="200000")
            env.update(extra)
            target = extra.get("TARGET", "inproc" if part == "analytic" else "pg")
            result = subprocess.run(["bash", str(suite), target, "test"],
                                    env=env, text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=10)
            should_fail = name not in ("inproc-fixtures", "pgbench-good", "oltp-inproc",
                                       "oltp-pg", "oltp-engine")
            assert (result.returncode != 0) == should_fail, (name, result.returncode, result.stdout)
            log = (data / "logs/suite-test.log").read_text()
            calls = (data / "calls").read_text().splitlines()
            logged = sum(" createdb " not in call for call in calls)
            assert log.count("stub stdout\n") == logged, (name, log, calls)
            assert log.count("stub stderr\n") == logged, (name, log, calls)
            if name == "inproc-fixtures":
                assert len(calls) == 4, calls
                for index, rows in [(0, 200000), (2, 2000000)]:
                    assert "analytic-load" in calls[index], calls
                    assert all(f"--rows {rows}" in calls[i] for i in (index, index + 1)), calls
            if part == "oltp":
                prefix = "inproc --label test" if target == "inproc" else f"bench {target} --label test"
                load = f"{prefix} oltp-load --rows 200000"
                query = f"{prefix} oltp --rows 200000 --secs 8 --clients"
                expected = [] if target == "inproc" else [f"{prefix} createdb --db bench"]
                expected += [load, f"{query} 1,8 --only point_select,point_select_simple,range_scan_100,secondary_lookup,select_1"]
                for clients in (1, 8):
                    for case in ("insert", "update_indexed", "upsert", "short_txn"):
                        expected += [load, f"{query} {clients} --only {case}"]
                if should_fail:
                    expected = expected[:int(extra["FAKE_AT"])]
                assert calls == expected, (name, calls, expected)
            print(f"{name}: passed (exit {result.returncode})")


if __name__ == "__main__":
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("suite.sh"))

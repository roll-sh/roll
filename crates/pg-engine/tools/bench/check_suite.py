#!/usr/bin/env python3
"""Exercise suite failure handling without starting services or running benchmarks."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


STUB = """#!/bin/bash
printf '%s\\n' "$*" >> "$BENCH_DATA/calls"
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
        shutil.copy2(suite, root / "suite.sh")
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
        for name, part, fake, extra in cases:
            data = root / name
            data.mkdir()
            env = dict(os.environ, BENCH_DATA=str(data), SUITE=part, FAKE=fake,
                       FAKE_FAILED="0", FAKE_INIT_EXIT="0", FAKE_TPS="100")
            env.update(extra)
            target = "inproc" if part == "analytic" else "pg"
            result = subprocess.run(["bash", str(root / "suite.sh"), target, "test"],
                                    env=env, text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=10)
            should_fail = name not in ("inproc-fixtures", "pgbench-good")
            assert (result.returncode != 0) == should_fail, (name, result.returncode, result.stdout)
            if name == "inproc-fixtures":
                calls = (data / "calls").read_text().splitlines()
                assert len(calls) == 4, calls
                for index, rows in [(0, 200000), (2, 2000000)]:
                    assert "analytic-load" in calls[index], calls
                    assert all(f"--rows {rows}" in calls[i] for i in (index, index + 1)), calls
            print(f"{name}: passed (exit {result.returncode})")


if __name__ == "__main__":
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("suite.sh"))

#!/usr/bin/env python3
"""Run the whole saved runner with fake git/docker/preflight/pnpm dependencies.

Only the W and V path assignments change in each execution copy. No service,
compiler, source test, or real Docker command is executed. Use a scratch copy.
"""
from pathlib import Path
import json
import os
import shlex
import subprocess

ROOT = Path(__file__).resolve().parent
GOOD = '  Running tests/e2e.rs (target/debug/deps/e2e-test)\nrunning 16 tests\ntest result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\npostgres: all green\n'
CASES = [
    ('success', GOOD, 0, 0, True),
    ('network-failure', GOOD, 0, 23, False),
    ('failed-process', GOOD, 101, 0, False),
    ('failed-summary', GOOD + 'test result: FAILED\n', 0, 0, False),
    ('error-line', GOOD + 'error: synthetic\n', 0, 0, False),
    ('compiler-error', GOOD + 'error[E0308]: synthetic\n', 0, 0, False),
    ('panic', GOOD + 'thread worker panicked at synthetic\n', 0, 0, False),
    ('stamp-skip', GOOD + 'no changes since the last green run\n', 0, 0, False),
    ('environment-skip', GOOD + 'skipped: set ROLL_E2E=1 to run\n', 0, 0, False),
    ('empty-e2e', GOOD.replace('16 passed', '0 passed'), 0, 0, False),
    ('ignored-e2e', GOOD.replace('0 ignored', '40 ignored'), 0, 0, False),
    ('no-summary', 'Running tests/e2e.rs (fake)\npostgres: all green\n', 0, 0, False),
    ('missing-green', GOOD.replace('postgres: all green\n', ''), 0, 0, False),
    ('wrong-binary', GOOD.replace('e2e.rs', 'unit.rs'), 0, 0, False),
    ('duplicate-e2e', GOOD + GOOD, 0, 0, False),
]
results = []
source = (ROOT / 'hardened.sh').read_text()
for name, log, rc, network_rc, accepted in CASES:
    folder = ROOT / 'cases' / name
    folder.mkdir(parents=True, exist_ok=True)
    work = folder / 'work'
    work.mkdir(exist_ok=True)
    out = folder / 'out'
    assert not out.exists(), 'copy proof to a fresh scratch directory to rerun'
    mocks = folder / 'mocks'
    mocks.mkdir(exist_ok=True)
    fixture = folder / 'pnpm.fixture'
    fixture.write_text(log)
    git = '#!/bin/sh\ncase "$*" in *"rev-parse HEAD"*) echo exact-source;; *"status --porcelain"*) :;; *) exit 99;; esac\n'
    pnpm = f'#!/bin/sh\nprintf started > {shlex.quote(str(folder / "PNPM-START"))}\ncat {shlex.quote(str(fixture))}\nexit {rc}\n'
    for command, body in [('git', git), ('pnpm', pnpm), ('sg', '#!/bin/sh\nexit 0\n'),
                          ('python3', f'#!/bin/sh\nprintf "synthetic network control\\n"\nexit {network_rc}\n')]:
        path = mocks / command
        path.write_text(body)
        path.chmod(0o755)
    script = source.replace('W=/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc', 'W=' + shlex.quote(str(work)))
    script = script.replace('V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/full-service', 'V=' + shlex.quote(str(out)))
    (folder / 'command.sh').write_text(script)
    env = dict(os.environ, PATH=str(mocks) + os.pathsep + os.environ['PATH'])
    run = subprocess.run(['bash', str(folder / 'command.sh'), 'exact-source'], env=env, capture_output=True)
    (folder / 'stdout').write_bytes(run.stdout)
    (folder / 'stderr').write_bytes(run.stderr)
    (folder / 'exit').write_text(f'{run.returncode}\n')
    marker = (out / 'status.tsv').exists() and 'COMPLETE\t0' in (out / 'status.tsv').read_text()
    started = (folder / 'PNPM-START').exists()
    assert (run.returncode == 0) == accepted and marker == accepted, (name, run.returncode, run.stderr)
    if network_rc:
        assert run.returncode == network_rc and not started
    elif rc:
        assert run.returncode == rc
    results.append(dict(case=name, exit=run.returncode, marker=marker, mocked_pnpm_started=started))
(ROOT / 'results.json').write_text(json.dumps(dict(status='PASS', controls=results, real_services_started=False), indent=2) + '\n')
print(f'PASS {len(results)} whole-runner mock controls; no real service executed')

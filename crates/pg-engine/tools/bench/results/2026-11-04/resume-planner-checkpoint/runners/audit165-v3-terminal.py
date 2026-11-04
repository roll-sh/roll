#!/usr/bin/env python3
"""Seal completed independent165 gates; never infer success from the wrapper alone."""
import datetime
import hashlib
import json
from pathlib import Path
import re
import subprocess

D = Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
E = D / 'check165ind-v3'
W = '/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4'
SOURCE = 'c8618834caf45bc0a34cf9985c07f9bc13b498ef'
STAGES = ['lifecycle', 'prepared', 'statistics', 'end-monitoring', 'debug-test', 'release-test', 'clippy', 'build', 'postgres-unit']

def git(*args):
    return subprocess.check_output(['git', '-C', W, *args], text=True).strip()

for pid in (3595120, 3595121, 3595148):
    assert not Path(f'/proc/{pid}').exists(), f'owned job still present: {pid}'
wrapper = (D / 'check165ind-v3-wrapper.log').read_text()
assert 'CHECK165INDEPENDENT-V3-PASS\n' in wrapper
assert re.search(r'^HEAVY-EXIT 0$', wrapper, re.M)
exits = [line.split('\t') for line in (E / 'exits.tsv').read_text().splitlines()]
assert exits == [[stage, '0'] for stage in STAGES], exits
assert git('rev-parse', 'HEAD') == (E / 'source.txt').read_text().strip() == SOURCE
assert git('rev-parse', 'HEAD^{tree}') == (E / 'tree.txt').read_text().strip()
assert not git('status', '--porcelain')
assert not (E / 'initial.status').read_bytes() and not (E / 'current.status').read_bytes()
for line in (D / 'ind165-v3-frozen-runner.sha256').read_text().splitlines():
    digest, path = line.split(maxsplit=1)
    assert hashlib.sha256(Path(path).read_bytes()).hexdigest() == digest, path
summaries = {}
for stage in STAGES:
    log = (E / f'{stage}.log').read_text()
    assert not re.search(r'^error(?:\[|:)|^test result: FAILED|^error: could not compile', log, re.M), stage
    if stage in ('debug-test', 'release-test'):
        out = E / f'{stage}-terminal-audit.json'
        subprocess.run(['python3', str(D / 'audit165-summary.py'), str(E / f'{stage}.log'),
                        str(D / 'ind165-v3-expected-per-binary.json'), str(out)], check=True)
        summaries[stage] = json.loads(out.read_text())['summary']
    elif stage in ('clippy', 'build'):
        assert re.search(r'Finished .*profile', log), stage
for stage, count in [('lifecycle', 2), ('prepared', 4), ('statistics', 1), ('end-monitoring', 1)]:
    log = (E / f'{stage}.log').read_text()
    assert f'test result: ok. {count} passed; 0 failed;' in log, stage
binary = Path('/home/meszmate/.cache/a165/bin/independent/roll-engine')
digest, path = (E / 'engine.sha256').read_text().strip().split(maxsplit=1)
assert path == str(binary) and hashlib.sha256(binary.read_bytes()).hexdigest() == digest
unit = json.loads((E / 'postgres-unit-status.json').read_text())
unitlog = (E / 'postgres-unit.log').read_text()
skip = 'postgres: no changes since the last green run; skipping' in unitlog
assert unit['status'] == ('SKIP' if skip else 'PASS')
assert skip or 'postgres: all green' in unitlog
assert unit['executed_unit_checks'] == (not skip)
assert unit['service_stack_acceptance'] is False and unit['pg_engine_execution'] is False
receipt = dict(
    observed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
    source=SOURCE, tree=git('rev-parse', 'HEAD^{tree}'), clean=True,
    owned_wrapper_heavy_runner_absent=True, frozen_runner_hashes_match=True,
    command_exits=dict(exits), test_summaries=summaries,
    engine_sha256=digest, postgres_unit=unit,
    resource_limits='Startupguard nice19/jobs2/testthreads<=2/affinity8-13,18-19; sampled descendants match, not exhaustive lifetime audit',
    resource_evidence=['startup-controls.json', 'affinity-observation-focused.json'],
    source_acceptance=False, performance_acceptance=False,
    remaining='R2 noHIGH/MEDIUM; older samePIDregistration displacement/fastpath assertion gap and otherLOWs retained. Parent review/integration and combined runtime/resource/service gates remain separate. Earlier focused wire101 preserved.')
(E / 'TERMINAL.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt))

"""Audit closed owner evidence without changing owner receipts or rerunning gates."""
import datetime
import hashlib
import json
from pathlib import Path
import re
import subprocess

C = Path('/home/meszmate/.cache/pg-engine-resume-20261104')
D = C / 'session_planner'
E = D / 'remaining171-lazy-v1'
W = '/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536'
SOURCE = 'cac0933bc4eb5ef87674889542cab271de4615d0'
TREE = 'bd48e5074887125072d7c23cb09c78fe6fe15c0b'

def git(repo, *args):
    return subprocess.check_output(['git', '-C', repo, *args]).decode().strip()

def digest(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

frozen_counts = {}
for name, count in [('full171-frozen.sha256', 9), ('remaining171-frozen.sha256', 11)]:
    rows = (D / name).read_text().splitlines()
    assert len(rows) == count
    for row in rows:
        expected, path = row.split('  ', 1)
        assert digest(path) == expected, path
    frozen_counts[name] = count

artifact = json.loads((E / 'build-artifact.json').read_text())
for key in ['binary', 'source_artifact']:
    assert digest(artifact[key]) == artifact['sha256']
    assert Path(artifact[key]).stat().st_size == artifact['bytes'] == 69451432
assert artifact['sha256'] == '02155aafc79c86e24696f4f0cae43b10c54774aac4b33c5d7f0dd767a3d06fb3'
assert artifact['source'] == SOURCE and artifact['profile'] == 'dist'
build = (E / 'build.log').read_text()
assert re.search(r'Compiling pg-engine .*agent-ad5ff4bf47e789536/crates/pg-engine', build)
assert 'Finished `dist` profile' in build
unit = (E / 'postgres-unit.log').read_text()
summaries = [tuple(map(int, m)) for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', unit)]
assert len(summaries) == 5 and [sum(m[i] for m in summaries) for i in range(3)] == [24, 0, 0]
assert 'postgres: all green' in unit
assert 'postgres: no changes since the last green run; skipping' not in unit
assert not re.search(r'^error:|test result: FAILED|panicked at', build + '\n' + unit, re.M)
for name, expected in [('exits.tsv', 'build\t0\npostgres-unit\t0\n'), ('audit-exits.tsv', 'build-audit\t0\npostgres-unit-audit\t0\n')]:
    assert (E / name).read_text() == expected
assert re.search(r'^HEAVY-EXIT 0$', (D / 'remaining171-lazy-v1-wrapper.log').read_text(), re.M)
assert re.search(r'^HEAVY-EXIT 1$', (D / 'full171-lazy-v1-wrapper.log').read_text(), re.M)
assert 'clippy-audit\t1' in (D / 'full171-lazy-v1/audit-exits.tsv').read_text()
original = json.loads((D / 'full171-lazy-v1/TERMINAL.json').read_text())
assert original['status'] == 'FAILED/PARTIAL' and original['wrapper_exit'] == 1
assert original['command_exits']['clippy'] == 0 and original['audit_exits']['clippy-audit'] == 1
for name in ['build', 'postgres-unit']:
    assert original['stages'][name]['status'].startswith('NOTRUN')
terminal = json.loads((E / 'TERMINAL.json').read_text())
assert terminal['remaining_status'] == 'SEPARATE DIST/UNIT COMPLETE' and terminal['problems'] == []
assert terminal['source'] == SOURCE and terminal['tree'] == TREE
assert git(W, 'rev-parse', 'HEAD') == SOURCE
assert git(W, 'rev-parse', 'HEAD^{tree}') == TREE
assert not git(W, 'status', '--porcelain')
assert git(str(Path.cwd()), 'rev-parse', 'HEAD') == 'fe7b0023c9df19a6dae1e6067ba8454998db48c5'
assert not git(str(Path.cwd()), 'status', '--porcelain')
groups = {1841329, json.loads((D / 'remaining171-launch.json').read_text())['process_group']}
live = []
for proc in Path('/proc').iterdir():
    if not proc.name.isdigit():
        continue
    try:
        if int((proc / 'stat').read_text().rsplit(')', 1)[1].split()[2]) in groups:
            live.append(int(proc.name))
    except (OSError, ValueError, IndexError):
        pass
assert not live, live
slot_pids = []
for path in Path('/tmp').glob('pg-engine-heavy.lock*/pid'):
    try:
        slot_pids.append(int(path.read_text().strip()))
    except (OSError, ValueError):
        pass
assert not ({1841334, 3890752} & set(slot_pids))
comparison = json.loads((C / 'planner171-clippy-reproducible-comparison.json').read_text())
assert comparison['same_message_path_multiset'] and comparison['diagnostic_count'] == 14
assert not comparison['extra'] and not comparison['missing']
receipt = dict(
    utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
    candidate=SOURCE, tree=TREE, parent_terminal_audit='PASS',
    standalone_gate_package='COMPLETE across original and separately authorized plans; Clippy accepted by separate raw-diagnostic comparison',
    original_pipeline='FAILED/PARTIAL; frozen Clippy audit1/HEAVY1; dist/unit NOTRUN there',
    separate_dist_unit='commands0/audits0; unit EXECUTED24/0/0/5',
    frozen_counts=frozen_counts, artifact=artifact,
    process_groups_absent=sorted(groups), owned_slots_released=True,
    audit_erratum=dict(
        prior_parent_audit='AssertionError before receipt write',
        cause='Expected HEAVY-EXIT: 1 instead of actual HEAVY-EXIT 1',
        correction='Exact anchored comparison to actual marker; no frozen inputs or gate results changed; no rerun'),
    retained_limits=['3 source-review LOWs', 'prior serial-bound cause unattributed', 'custom ANY/IN gaps'],
    integration_acceptance=False, service_acceptance=False, performance_acceptance=False,
    shared_source_unchanged=True,
)
(C / 'planner171-cac-remaining-parent-terminal-audit.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt, indent=2))

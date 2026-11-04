#!/usr/bin/env python3
"""Export completed recovery evidence using a temporary Git index; leave pinned jobs untouched."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

W = '/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4'
D = Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
PREFIX = 'crates/pg-engine/tools/bench/results/2026-11-04/resume-planner-checkpoint/'
REF = 'refs/heads/wip/resume-20261104-planner-evidence'

def git(*args, data=None, env=None):
    return subprocess.check_output(['git', '-C', W, *args], input=data, env=env).decode().strip()

readme = '''# Recovery checkpoint (2026-11-04)

Evidence only. No candidate source is accepted or performance advantage claimed.
All three original worktrees and histories were inspected. The T3 bindings pointed to the
parent checkout; all implementation Git operations instead used the original worktrees.

OLTP165 recovered clean at 6fde4096. jobA failed (including an executable-copy failure).
jobB's final HEAVY-EXIT 0 is FALSE SUCCESS: every cargo step failed on a missing AtomicU64
import. Its debug/release failure logs are preserved. baeee87a fixes only that import;
complete current-source debug validation has now passed446tests,0failed,18ignored.
The complete release suite also passed446tests,0failed,18ignored across60summaries.
Release all-target Clippy exited0 with warnings; dist build, PostgreSQL and footprint/performance gates remain pending.
PostgreSQL18.6 recording of a165_cursor_stream succeeded: nonempty 397-line expected output.
The other two golden tests return early in recording mode: this is recording evidence,
not itself a successful embedded/wire/shuffled comparison. The subsequent complete
debug suite did pass all three modes; its completed log is exported separately. Snapshot 2ef850e8 retains this golden.

Historical matrix m0/m1 contain partial campaigns and lack raw pgbench output and a
complete current-source gate set. They cannot establish a performance regression or gain.
Old baseline/pre footprint outputs do not verify baeee87a. Runner snapshots are exported for review. Only completed commands with individual exit
logs are reported as executed: check165 remains in progress; check171 and bench165 have
not started. The frozen benchmark plan precedes every new measurement.

Planner171 recovered at 5c312235 with six dirty SQL files; their exact snapshot is ec5b9f39.
An earlier cargo check succeeded, but the log lacks sufficient source identity to accept
current source. Chosen-cost recording and custom-plan use remain this owner's scope.

FDW172 recovered clean at 63f7c0b4, no round8 implementation. Baseline dist and contrib
completed. Report: postgres_fdw1945/2053, dblink218/218, file_fdw187/187, aggregate2350/2458.
This is baseline evidence only and does not establish an improvement.

Owner backups were checked with ls-remote: baeee87a wip/a165-resume-20261104,
2ef850e8 wip/a165-resume-golden-20261104, ec5b9f39 wip/a171-resume-20261104,
63f7c0b4 wip/a172-resume-20261104. Shared branch and parent/main worktrees were not mutated.

The manifest records exact hashes of every exported artifact. This checkpoint includes the completed debug-suite output and exact testedtree.
Completed release output, command and summary are included. The still-running dist output is excluded.
'''
files = {'README.md': readme.encode(), 'export_checkpoint.py': Path(__file__).read_bytes()}
sources = {
    'runners/audit-footprint.py': str(D / 'audit-footprint.py'),
    'oltp165/release-complete.log': str(D / 'check165/release-test.log'),
    'oltp165/release.command': str(D / 'check165/release-test.command'),
    'oltp165/release-summary.json': str(D / 'check165/release-summary.json'),
    'oltp165/clippy-complete.log': str(D / 'check165/clippy.log'),
    'oltp165/clippy.command': str(D / 'check165/clippy.command'),
    'runners/check165.sh': str(D / 'check165.sh'),
    'runners/check171.sh': str(D / 'check171.sh'),
    'runners/bench165.sh': str(D / 'bench165.sh'),
    'runners/BENCH-PLAN.md': str(D / 'BENCH-PLAN.md'),
    'runners/matrix.sh': '/home/meszmate/.cache/a165/scripts/matrix.sh',
    'runners/conn1000.sh': '/home/meszmate/.cache/a165/scripts/conn1000.sh',
    'runners/footprint.sh': '/home/meszmate/.cache/a165/scripts/footprint.sh',
    'runners/jobA.sh': '/home/meszmate/.cache/a165/scripts/jobA.sh',
    'runners/jobB.sh': '/home/meszmate/.cache/a165/scripts/jobB.sh',
    'runners/jobB-original-false-success.sh': str(D / 'jobB-original-false-success.sh'),
    'oltp165/debug-complete.log': str(D / 'check165/debug-test.log'),
    'oltp165/debug.command': str(D / 'check165/debug-test.command'),
    'oltp165/debug-summary.json': str(D / 'check165/debug-summary.json'),
    'oltp165/debug-tested-tree.txt': str(D / 'check165/debug-tested-tree.txt'),
    'oltp165/reference.txt': str(D / 'check165/reference.txt'),
    'oltp165/jobA.log': '/home/meszmate/.cache/a165/logs/jobA.log',
    'oltp165/jobB.log': '/home/meszmate/.cache/a165/logs/jobB.log',
    'oltp165/test-debug-failed.log': '/home/meszmate/.cache/a165/logs/test-debug.out',
    'oltp165/test-release-failed.log': '/home/meszmate/.cache/a165/logs/test-release.out',
    'oltp165/m0.tsv': '/home/meszmate/.cache/a165/logs/m0.tsv',
    'oltp165/m1.tsv': '/home/meszmate/.cache/a165/logs/m1.tsv',
    'oltp165/fp-base.log': '/home/meszmate/.cache/a165/logs/fp-base.log',
    'oltp165/fp-prebase.log': '/home/meszmate/.cache/a165/logs/fp-prebase.log',
    'oltp165/record.log': str(D / 'check165/record.log'),
    'oltp165/record.command': str(D / 'check165/record.command'),
    'oltp165/record.sha256': str(D / 'check165/record.sha256'),
    'oltp165/record.patch': str(D / 'check165/record.patch'),
    'oltp165/record-source.txt': str(D / 'check165/source.txt'),
    'planner171/check.log': '/home/meszmate/.cache/a171/logs/check.log',
    'planner171/base-dist.log': '/home/meszmate/.cache/a171/logs/base-dist.log',
    'fdw172/base-engine.rev': '/home/meszmate/.cache/a172/base-engine.rev',
    'fdw172/base-dist.log': '/home/meszmate/.cache/a172/base-dist.log',
    'fdw172/contrib-base.log': '/home/meszmate/.cache/a172/contrib-base.log',
    'fdw172/REPORT.md': '/home/meszmate/.cache/a172/contrib/run-base/report/REPORT.md',
}
for name, source in sources.items():
    files[name] = Path(source).read_bytes()
files['manifest.json'] = (json.dumps({name: {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}
    for name, data in sorted(files.items())}, indent=2) + '\n').encode()
with tempfile.TemporaryDirectory(prefix='a165-evidence-') as temp:
    env = dict(os.environ, GIT_INDEX_FILE=temp + '/index')
    head = git('rev-parse', 'HEAD')
    git('read-tree', head, env=env)
    for name, data in files.items():
        blob = git('hash-object', '-w', '--stdin', data=data)
        git('update-index', '--add', '--cacheinfo', '100644', blob, PREFIX + name, env=env)
    tree = git('write-tree', env=env)
    previous = git('ls-remote', 'origin', REF).split()
    parents = ['-p', previous[0], '-p', head] if previous else ['-p', head]
    commit = git('commit-tree', tree, *parents, data=b'pg-engine: preserve completed OLTP, planner, FDW recovery evidence\n')
    git('push', 'origin', commit + ':' + REF)
    observed = git('ls-remote', 'origin', REF).split()[0]
    assert observed == commit, (commit, observed)
    (D / 'checkpoint-commit.txt').write_text(commit + ' ' + REF + '\n')
    print(commit, REF, len(files), 'artifacts; remote SHA confirmed')

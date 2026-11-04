#!/usr/bin/env python3
"""Verify a closed committed owner evidence bundle without checking it out."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import subprocess

p = argparse.ArgumentParser()
p.add_argument('commit')
p.add_argument('remote_ref')
p.add_argument('receipt')
p.add_argument('--previous')
a = p.parse_args()
repo = '/home/meszmate/worktrees/roll/t3-d4020b98'
prefix = 'crates/pg-engine/tools/bench/results/2026-11-04/resume-planner-checkpoint/'
def git(*args):
    return subprocess.check_output(['git', '-C', repo, *args])
ids = {}
for row in git('ls-tree', '-rz', a.commit, '--', prefix).split(b'\0'):
    if row:
        meta, name = row.split(b'\t')
        ids[name.decode()[len(prefix):]] = meta.split()[2].decode()
proc = subprocess.Popen(['git', '-C', repo, 'cat-file', '--batch'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
data = {}
for name, oid in ids.items():
    proc.stdin.write((oid + '\n').encode())
    proc.stdin.flush()
    header = proc.stdout.readline().split()
    data[name] = proc.stdout.read(int(header[2]))
    assert proc.stdout.read(1) == b'\n'
proc.stdin.close()
assert proc.wait() == 0
manifest = json.loads(data['manifest.json'])
assert set(data) == set(manifest) | {'manifest.json'}
for name, record in manifest.items():
    assert len(data[name]) == record['bytes'], name
    assert hashlib.sha256(data[name]).hexdigest() == record['sha256'], name
assert git('ls-remote', 'origin', a.remote_ref).decode().split()[0] == a.commit
changed = None
if a.previous:
    changed = git('diff', '--name-only', a.previous, a.commit).decode().splitlines()
    assert all(name.startswith(prefix) for name in changed)
result = {'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'commit': a.commit, 'remote_exact': True, 'closed_files': len(data), 'hashes_verified': len(manifest), 'evidence_only_changed_paths': len(changed) if changed is not None else None, 'source_acceptance': False}
Path(a.receipt).write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result))

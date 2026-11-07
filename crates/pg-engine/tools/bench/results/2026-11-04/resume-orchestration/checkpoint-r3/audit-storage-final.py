#!/usr/bin/env python3
"""Independently verify storage169's sealed final raw objects and outcomes."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
ROOT = Path(__file__).resolve().parent
COMMIT = '25d8cd077c6fa658a578f3434ff896798577b0e3'
PREFIX = 'crates/pg-engine/results/2026-11-04/resume-owner169-raw-seal2/'
SOURCE = '79746f3174996b8047c4c0c30db87f345418b162'


def git(*args):
    return subprocess.check_output(['git', *args], cwd=REPO)


def blobs(specs):
    assert specs
    data = subprocess.check_output(
        ['git', 'cat-file', '--batch'], cwd=REPO,
        input=('\n'.join(specs) + '\n').encode())
    pos = 0
    out = []
    for spec in specs:
        end = data.index(b'\n', pos)
        fields = data[pos:end].split()
        assert len(fields) == 3 and fields[1] == b'blob', (spec, fields)
        size = int(fields[2])
        start = end + 1
        out.append((fields[0].decode(), data[start:start + size]))
        pos = start + size + 1
    assert pos == len(data)
    return out


manifest = json.loads(git('show', f'{COMMIT}:{PREFIX}manifest.json'))
entries = manifest['files']
objects = blobs([f"{COMMIT}:{entry['target']}" for entry in entries])
payloads = {}
for entry, (oid, data) in zip(entries, objects):
    assert entry['target'].startswith(PREFIX)
    assert len(data) == entry['bytes']
    assert hashlib.sha256(data).hexdigest() == entry['sha256']
    assert oid == entry['git_blob']
    payloads[entry['target'][len(PREFIX):]] = data
listed = {entry['target'] for entry in entries}
tree = set(git('ls-tree', '-r', '--name-only', COMMIT, '--', PREFIX).decode().splitlines())
assert tree == listed | {PREFIX + 'manifest.json'}
remote = git('ls-remote', 'origin', 'refs/heads/wip/resume-20261104-a169-raw-seal2').decode().strip()
assert remote.split()[0] == COMMIT
base = 'cache-raw/a169-final/'
assert payloads[base + 'source-head'].decode().strip() == SOURCE
assert not payloads[base + 'source-diff'].strip()
exits = payloads[base + 'exits'].decode().splitlines()
assert len(exits) == 14 and all(line.endswith(' EXIT 0') for line in exits)
expected = {'proof-debug': [4, 1], 'storage-debug': [29],
            'integrations-debug': [15, 2, 1, 4, 1],
            'proof-release': [4, 1, 1], 'crash-release': [2],
            'storage-release': [29]}
counts = {}
for name, want in expected.items():
    data = payloads[base + name + '.log'].decode()
    summaries = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', data)
    assert [int(row[0]) for row in summaries] == want, (name, summaries)
    assert all(row[1:] == ('0', '0') for row in summaries)
    assert not re.search(r'test result: FAILED|^error:|panicked at', data, re.M)
    counts[name] = [int(row[0]) for row in summaries]
assert re.search(rb'Finished .*release', payloads[base + 'build.log'])
assert not re.search(rb'^error:', payloads[base + 'clippy.log'], re.M)
source_rows = [line.split('  ', 1) for line in payloads[base + 'source-sha256'].decode().splitlines()]
source_objects = blobs([f'{SOURCE}:{path}' for _, path in source_rows])
for (want, path), (_, data) in zip(source_rows, source_objects):
    assert hashlib.sha256(data).hexdigest() == want, path
report = dict(status='PASS', commit=COMMIT, remote=remote, payload_blobs=len(entries),
              manifest_blob=1, closed_set=True, source=SOURCE,
              source_manifest_entries=len(source_rows), source_bytes_match_commit=True,
              command_exits=exits, test_summaries=counts,
              scope='committed raw blobs, source hashes, command exits, expected test totals, build/clippy evidence; no new suite or timing',
              performance='HELD', combined_service='HELD')
(ROOT / 'storage-final-parent-audit.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({key: report[key] for key in ['status', 'payload_blobs', 'source_manifest_entries', 'test_summaries']}))

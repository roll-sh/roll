#!/usr/bin/env python3
"""Verify sealed owner payloads from committed blobs, with no checkout writes."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import subprocess

REPO = '/home/meszmate/worktrees/roll/t3-d4020b98'
BUNDLES = [
    ('76480426493b24ef0de97f6d43c3cd6db2cd30d3', 'wip/resume-20261104-a163-review-cic-final2', 'crates/pg-engine/results/2026-11-04/resume-owner163-review-cic-final2'),
    ('915b51b561f57a865c52e19f3aebf4fdab2d5706', 'wip/resume-20261104-a166', 'crates/pg-engine/results/2026-11-04/resume-owner166/followup-keys'),
    ('915b51b561f57a865c52e19f3aebf4fdab2d5706', 'wip/resume-20261104-a166', 'crates/pg-engine/results/2026-11-04/resume-owner166/review-repro'),
]

def git(*args):
    return subprocess.check_output(['git', '-C', REPO, *args])

def audit():
    before = (git('rev-parse', 'HEAD'), git('status', '--porcelain=v1'))
    results = []
    for commit, ref, prefix in BUNDLES:
        remote = git('ls-remote', '--heads', 'origin', ref).decode().strip()
        assert remote == commit + '\trefs/heads/' + ref, remote
        manifest_path = prefix + '/manifest.json'
        raw = git('show', commit + ':' + manifest_path)
        manifest = json.loads(raw)
        if isinstance(manifest['files'], list):
            records = manifest['files']
        else:
            records = [{'path': prefix + '/' + path, 'sha256': sha} for path, sha in manifest['sha256'].items()]
            assert len(records) == manifest['files']
        expected = [item['path'] for item in records]
        assert len(set(expected)) == len(expected), 'duplicate manifest paths'
        for path in expected:
            assert path.startswith(prefix + '/') and '..' not in PurePosixPath(path).parts, path
        actual = set(git('ls-tree', '-r', '--name-only', commit, '--', prefix).decode().splitlines())
        assert actual == set(expected) | {manifest_path}, {'missing': sorted(set(expected) - actual), 'extra': sorted(actual - set(expected) - {manifest_path})}
        total = 0
        for record in records:
            payload = git('show', commit + ':' + record['path'])
            assert hashlib.sha256(payload).hexdigest() == record['sha256'], record['path']
            if 'bytes' in record:
                assert len(payload) == record['bytes'], record['path']
            total += len(payload)
        results.append(dict(commit=commit, ref=ref, manifest=manifest_path, manifest_sha256=hashlib.sha256(raw).hexdigest(), payloads=len(records), bytes=total, closed_membership=True, remote_exact=True))
    assert before == (git('rev-parse', 'HEAD'), git('status', '--porcelain=v1'))
    return dict(status='PASS', bundles=results, payloads=sum(item['payloads'] for item in results), missing=0, mismatch=0, shared_head=before[0].decode().strip(), shared_status_unchanged=True)

if __name__ == '__main__':
    result = audit()
    output = Path(__file__).with_name('final-owner-payloads-parent-audit.json')
    assert not output.exists(), output
    output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))

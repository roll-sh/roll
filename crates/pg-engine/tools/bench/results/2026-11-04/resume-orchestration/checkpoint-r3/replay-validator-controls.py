#!/usr/bin/env python3
"""Replay saved parser controls into a new external output directory."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys

ROOT = Path(__file__).resolve().parent / 'validator-controls'
OUT = Path(sys.argv[1]).resolve()
OUT.mkdir(parents=True, exist_ok=False)
results = []
array = ROOT / 'array-validator-proof'
for case in json.loads((array / 'proof.json').read_text())['cases']:
    source = array / (case['case'] + '.log')
    assert hashlib.sha256(source.read_bytes()).hexdigest() == case['sha256']
    command = [sys.executable, str(ROOT / 'audit-array.py'), str(source), str(array / 'array-header.sql')]
    result = subprocess.run(command, capture_output=True)
    assert result.returncode == case['expected_exit'], case['case']
    for suffix, content in [('stdout', result.stdout), ('stderr', result.stderr)]:
        (OUT / (case['case'] + '.' + suffix)).write_bytes(content)
    results.append(dict(case=case['case'], command=command, exit=result.returncode,
                        input_sha256=case['sha256']))
version = ROOT / 'version-matcher-proof'
for case in json.loads((version / 'proof.json').read_text())['cases']:
    source = version / (case['case'] + '.log')
    command = [*case['command'][:-1], str(source)]
    result = subprocess.run(command, capture_output=True)
    assert result.returncode == case['expected_exit'], case['case']
    for suffix, content in [('stdout', result.stdout), ('stderr', result.stderr)]:
        (OUT / (case['case'] + '.' + suffix)).write_bytes(content)
    results.append(dict(case=case['case'], command=command, exit=result.returncode,
                        input_sha256=hashlib.sha256(source.read_bytes()).hexdigest()))
(OUT / 'results.json').write_text(json.dumps(dict(status='PASS', cases=results, services_started=False), indent=2) + '\n')
print(f'PASS {len(results)} saved-byte controls; no services')

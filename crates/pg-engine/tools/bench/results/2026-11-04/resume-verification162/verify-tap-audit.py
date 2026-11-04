#!/usr/bin/env python3
"""Replay the preserved TAP audit controls and real passing/failing fixtures."""
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent
checker = root / 'audit-tap.py'
controls = json.loads((root / 'tap-audit-proof.json').read_text())
results = []
for control in controls:
    name = control['name']
    expected = control.get('expected_exit', control['actual_exit'])
    if name in ('real166-false-Rust-pass', 'real162-passing-14817', 'missing-raw-files'):
        label = 'negative166' if name == 'real166-false-Rust-pass' else 'historical162pass'
        raw = root / 'tap-proof-raw' / label
        args = ['pg-dump', str(raw / 'wrapper.log'),
                str(root / 'tap-controls' / 'missing-directory') if name == 'missing-raw-files' else str(raw)]
    else:
        args = ['tap', str(root / 'tap-controls' / f'{name}.tap'), '--expected', '2']
    run = subprocess.run([sys.executable, str(checker), *args], capture_output=True, text=True)
    assert run.returncode == expected, (name, run.returncode, run.stdout, run.stderr)
    result = json.loads(run.stdout)
    assert (result['status'] == 'PASS') == (expected == 0), name
    results.append({'name': name, 'expected_exit': expected, 'actual_exit': run.returncode})
print(json.dumps({'status': 'PASS', 'cases': results}, indent=2))

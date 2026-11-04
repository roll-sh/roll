#!/usr/bin/env python3
"""Read-only source/evidence capture; no engine, cargo, Docker, or checkout edits."""
import hashlib
import json
from pathlib import Path
import subprocess

root = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt = Path('/home/meszmate/roll/.claude/worktrees/agent-a814fa71b51112148')
out = root / 'scan149-localization'
out.mkdir(exist_ok=True)

def git(*args):
    return subprocess.check_output(['git', '-C', str(wt), *args])

files = ['exec/scan.rs', 'exec/dml.rs', 'storage/mod.rs', 'storage/keys.rs',
         'storage/hot.rs', 'storage/row.rs', 'storage/sst.rs', 'txn/mod.rs']
before = {'head': git('rev-parse', 'HEAD').decode().strip(),
          'tree': git('rev-parse', 'HEAD^{tree}').decode().strip(),
          'status': git('status', '--porcelain=v1').decode(),
          'patch_sha256': hashlib.sha256(git('diff', 'HEAD')).hexdigest()}
assert before['head'] == '2a854a48074dbd3317807f634a773f29540b2673'
assert not before['status']
identity = {str(wt / 'crates/pg-engine/src' / f):
            hashlib.sha256((wt / 'crates/pg-engine/src' / f).read_bytes()).hexdigest()
            for f in files}
(out / 'source-identity.json').write_text(json.dumps({'source': before, 'sha256': identity}, indent=2)+'\n')
(out / 'candidate-vs-f0563cd8.patch').write_bytes(git('diff', 'f0563cd8', '2a854a48', '--', 'crates/pg-engine/src'))

# Exact byte-order witness of the checked-in mvcc_key formula and cursor bound.
# This is not execution of the Rust Store and does not establish failure attribution.
k = bytes.fromhex('0000000700000000018000000000000001')
bad_end = k + b'\0'
good_end = k[:-1] + bytes([k[-1] + 1])
witness = []
for ts in (0, 1, 42, (1 << 64) - 1):
    physical = k + (((1 << 64) - 1) ^ ts).to_bytes(8, 'big')
    row = {'ts': ts, 'user_key': k.hex(), 'physical_key': physical.hex(),
           'bad_end': bad_end.hex(), 'prefix_end': good_end.hex(),
           'cold_cursor_includes': k <= physical < bad_end,
           'prefix_end_includes': k <= physical < good_end,
           'hot_user_range_includes': k <= k < bad_end}
    assert not row['cold_cursor_includes']
    assert row['prefix_end_includes'] and row['hot_user_range_includes']
    witness.append(row)
(out / 'byte-order-witness.json').write_text(json.dumps({
    'method': 'Python byte-order witness of source formula; no Rust Store execution',
    'exit': 0, 'cases': witness}, indent=2)+'\n')

ranges = {'exec/scan.rs': [(3385,3447),(4610,4620),(4860,4914)],
          'exec/dml.rs': [(4844,4850),(4898,4946)],
          'storage/mod.rs': [(67,73),(870,894),(2050,2083),(2145,2161)],
          'storage/hot.rs': [(33,36),(117,130),(167,196)],
          'txn/mod.rs': [(2474,2484),(2650,2670)],
          'storage/keys.rs': [(425,436)]}
parts = []
for name, spans in ranges.items():
    lines = (wt / 'crates/pg-engine/src' / name).read_text().splitlines()
    for lo, hi in spans:
        parts.append(f'### crates/pg-engine/src/{name}:{lo}\n')
        parts.extend(f'{i}: {lines[i-1]}\n' for i in range(lo, hi+1))
(out / 'source-excerpts.txt').write_text(''.join(parts))
assert git('rev-parse', 'HEAD').decode().strip() == before['head']
assert git('status', '--porcelain=v1').decode() == before['status']
print(json.dumps({'exit': 0, 'source': before, 'witness_cases': len(witness),
                  'runtime_probe_run': False, 'source_unchanged': True}))

#!/usr/bin/env python3
"""Compare diagnostic identities, rather than compilation copies or primary spans."""
import argparse
import collections
import hashlib
import json
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('cargo_log')
p.add_argument('reference')
p.add_argument('output')
a = p.parse_args()
raw = Path(a.cargo_log).read_bytes()
events = []
for line in raw.decode().splitlines():
    try:
        events.append(json.loads(line))
    except json.JSONDecodeError:
        pass
assert any(e.get('reason') == 'build-finished' and e.get('success') for e in events)
messages = [e for e in events if e.get('reason') == 'compiler-message']
assert not any(e['message']['level'] == 'error' for e in messages)
warnings = [e for e in messages if e['message']['level'] == 'warning']
unique = {}
primary_count = 0
for e in warnings:
    m = e['message']
    spans = [s for s in m.get('spans', []) if s.get('is_primary')]
    primary_count += len(spans)
    paths = {s['file_name'] for s in spans}
    assert len(paths) == 1, (m['message'], paths)
    locations = tuple((s['file_name'], s['byte_start'], s['byte_end']) for s in spans)
    identity = (e['package_id'], e['target']['name'], tuple(e['target']['kind']), (m.get('code') or {}).get('code'), m['message'], locations)
    unique[identity] = {'message': m['message'], 'path': next(iter(paths)), 'primary_locations': locations}
reference = json.loads(Path(a.reference).read_text())['diagnostics']
got = collections.Counter((r['message'], r['path']) for r in unique.values())
want = collections.Counter((r['message'], r['path']) for r in reference)
result = {'scope': 'Separate post-hoc comparison; never changes a frozen gate result', 'raw_sha256': hashlib.sha256(raw).hexdigest(), 'raw_warning_messages': len(warnings), 'raw_primary_span_entries': primary_count, 'duplicate_diagnostic_copies': len(warnings) - len(unique), 'diagnostic_count': len(unique), 'diagnostics': list(unique.values()), 'same_message_path_multiset': got == want, 'extra': list((got - want).elements()), 'missing': list((want - got).elements())}
Path(a.output).write_text(json.dumps(result, indent=2) + '\n')
assert got == want, result
print(json.dumps({k: v for k, v in result.items() if k != 'diagnostics'}))
